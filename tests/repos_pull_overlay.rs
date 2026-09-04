//! Differential parity tests for `_pull_overlay`
//! (`lib/dot/repos/pull.sh`) against the live shell: the missing-checkout
//! clone, the worktree/origin guards, the upstream fetch, the generation
//! fast path, candidate validation, the parent snapshot, the pull, and
//! mode normalization, across plain, optional, and counted-UI rows.
//!
//! Separate binary because the rows drive real `git clone`/`git pull`
//! runs: each side builds its own origin plus overlay under disjoint
//! directories, so paths and hashes normalize before comparing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::progress_ui::Palette;
use dot::repos_base::{Base, Topology};
use dot::repos_overlays::DestinationInputs;
use dot::repos_pull_overlay::{PullOverlayInputs, pull_overlay};
use dot::repos_pull_queries::CandidateEnv;
use dot::test_support::TempDir;

/// Sources for the overlay-pull chapter.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
    ". \"$1/lib/dot/repos/config.sh\"\n",
    ". \"$1/lib/dot/repos/overlays.sh\"\n",
    ". \"$1/lib/dot/reserved.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/progress-ui.sh\"\n",
    ". \"$1/lib/dot/run.sh\"\n",
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

/// One twin side: an origin repo plus a clone at `$HOME/overlay`.
struct Side {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    origin: PathBuf,
    origin_text: String,
    overlay: PathBuf,
    overlay_text: String,
    manifest: String,
    legacy: String,
}

impl Side {
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
        Side {
            _dir: dir,
            home,
            home_text: home_text.clone(),
            origin,
            origin_text,
            overlay,
            overlay_text,
            manifest: format!("{home_text}/manifest.tsv"),
            legacy: format!("{home_text}/legacy.tsv"),
        }
    }
}

/// Which URL the row pulls.
#[derive(Clone, Copy)]
enum Url {
    /// Empty URL.
    Missing,
    /// The side origin.
    Origin,
    /// A path that cannot clone.
    Bad,
}

/// Per-row fixture after the seed clone.
fn setup_side(side: &Side, tag: &str) {
    match tag {
        "missing-url"
        | "missing-url-optional"
        | "clone"
        | "clone-optional"
        | "clone-ui"
        | "clone-fail"
        | "clone-fail-optional" => {
            std::fs::remove_dir_all(&side.overlay).expect("remove overlay");
        }
        "not-worktree" => {
            std::fs::remove_dir_all(&side.overlay).expect("remove overlay");
            stage(&side.overlay, "user.txt", b"user data\n");
        }
        "origin-mismatch" | "origin-mismatch-ui" => {
            git(
                &side.overlay,
                &["remote", "set-url", "origin", "/elsewhere"],
            );
        }
        "skipped" | "skipped-ui" => {
            git(&side.overlay, &["branch", "--unset-upstream"]);
        }
        "changed" | "optional-changed" => {
            stage(&side.origin, "home/newfile.txt", b"from origin\n");
            commit(&side.origin, "add newfile");
        }
        "conflict-backup" => {
            stage(&side.origin, "home/clash.txt", b"origin clash\n");
            commit(&side.origin, "add clash");
            stage(&side.overlay, "home/clash.txt", b"user clash\n");
        }
        "diverged" => {
            // Pin the marker style repo-locally on both twins: the
            // aftermath compares the engines, not ambient
            // `merge.conflictStyle` (e.g. a user `zdiff3`).
            git(&side.overlay, &["config", "merge.conflictStyle", "merge"]);
            stage(&side.overlay, "home/overlay.txt", b"home change\n");
            commit(&side.overlay, "home change");
            stage(&side.origin, "home/overlay.txt", b"origin change\n");
            commit(&side.origin, "origin change");
        }
        "invalid-candidate" => {
            stage(&side.origin, "home/.dotfiles/evil", b"x\n");
            commit(&side.origin, "add evil");
        }
        "current" | "current-ui" | "optional-current" => {}
        _ => unreachable!("unknown row {tag}"),
    }
}

/// Aftermath probes per row.
fn probe_rels(tag: &str) -> &'static [&'static str] {
    match tag {
        "clone" | "clone-optional" | "clone-ui" => &[],
        "changed" | "optional-changed" => &["home/newfile.txt"],
        "conflict-backup" => &["home/clash.txt"],
        "diverged" => &["home/overlay.txt"],
        "invalid-candidate" => &["home/.dotfiles/evil"],
        _ => &[],
    }
}

/// Shell aftermath probe: one `st=` line per rel, the overlay
/// presence for clone rows, plus the backup entry count for the
/// conflict row.
fn shell_probe(side: &Side, tag: &str) -> String {
    let mut out = String::new();
    if tag == "clone" || tag == "clone-optional" || tag == "clone-ui" {
        out.push_str(&format!(
            "if [[ -d {} ]]; then printf 'overlay=present\\n'; else printf 'overlay=absent\\n'; fi; ",
            sq(&side.overlay_text),
        ));
    }
    for rel in probe_rels(tag) {
        out.push_str(&format!(
            "p={}; if [[ -f \"$p\" ]]; then printf 'st={rel}:file:%s\\n' \"$(cat \"$p\")\"; \
             else printf 'st={rel}:absent\\n'; fi; ",
            sq(&format!("{}/overlay/{rel}", side.home_text)),
        ));
    }
    if tag == "conflict-backup" {
        out.push_str(&format!(
            "n=0; if [[ -d {} ]]; then for e in {}/*; do [[ -e \"$e\" ]] && n=$((n + 1)); done; fi; printf 'backups=%s\\n' \"$n\"\n",
            sq(&format!("{}/.dot-backup/pull", side.home_text)),
            sq(&format!("{}/.dot-backup/pull", side.home_text)),
        ));
    }
    out
}

/// Shell preamble: home, UI/quiet/verbose flags, empty rollback and
/// overlay records, manifests, and the candidate-validation
/// environment; the topology pins after sourcing because model.sh
/// detection runs at load.
fn shell_preamble(side: &Side, ui_total: Option<&str>, quiet: bool, verbose: bool) -> String {
    let home_text = &side.home_text;
    format!(
        "export HOME={h} DOT_QUIET={q} DOT_VERBOSE={v} XDG_STATE_HOME={h}/.local/state SHDEPS_INSTALL_DIR={h}/.local/share; \
         {u}DOT_OVERLAY_ROLLBACK_PATHS=(); DOT_OVERLAY_ROLLBACK_TARGETS=(); OVERLAYS=(); ACTIVE_OVERLAYS=(); \
         DOT_OVERLAY_MANIFEST={m} DOT_OVERLAY_LEGACY_MANIFEST={l}; DOT_BASE_TOPOLOGY=ordinary; ",
        h = sq(home_text),
        q = u8::from(quiet),
        v = u8::from(verbose),
        u = ui_total
            .map(|total| format!("export DOT_UI_TOTAL={total}; "))
            .unwrap_or_default(),
        m = sq(&side.manifest),
        l = sq(&side.legacy),
    )
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

/// Candidate environment mirroring the shell preamble.
fn candidate_env(side: &Side) -> CandidateEnv {
    let home = side.home_text.clone();
    CandidateEnv {
        home: home.clone(),
        checkout: format!("{home}/.local/share/cgraf78/dot"),
        pwd: home.clone(),
        source_root: env!("CARGO_MANIFEST_DIR").to_string(),
        state_home: format!("{home}/.local/state"),
        install_root: format!("{home}/.local/share"),
        provider_state: format!("{home}/.local/state/shdeps"),
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

/// Rust aftermath dump mirroring [`shell_probe`].
fn rust_probe(side: &Side, tag: &str) -> String {
    let mut out = String::new();
    if tag == "clone" || tag == "clone-optional" || tag == "clone-ui" {
        out.push_str(if side.overlay.is_dir() {
            "overlay=present\n"
        } else {
            "overlay=absent\n"
        });
    }
    for rel in probe_rels(tag) {
        let path = side.overlay.join(rel);
        match std::fs::read(&path) {
            Ok(bytes) => out.push_str(&format!(
                "st={rel}:file:{}\n",
                String::from_utf8_lossy(&bytes).trim_end_matches('\n')
            )),
            Err(_) => out.push_str(&format!("st={rel}:absent\n")),
        }
    }
    if tag == "conflict-backup" {
        let count = std::fs::read_dir(side.home.join(".dot-backup/pull"))
            .map(|entries| entries.filter_map(|entry| entry.ok()).count())
            .unwrap_or(0);
        out.push_str(&format!("backups={count}\n"));
    }
    out
}

/// Run one row on twin sides and compare status, rc, both streams,
/// and aftermath. The shell always exits 0; only the status varies.
#[allow(clippy::too_many_arguments)]
fn check_row(
    tag: &str,
    optional: bool,
    ui_total: Option<&str>,
    quiet: bool,
    verbose: bool,
    url: Url,
    want_status: &str,
) {
    let shell_side = Side::build(&format!("{tag}-shell"));
    let rust_side = Side::build(&format!("{tag}-rust"));
    setup_side(&shell_side, tag);
    setup_side(&rust_side, tag);
    let url_text = match url {
        Url::Missing => String::new(),
        Url::Origin => shell_side.origin_text.clone(),
        Url::Bad => "/nonexistent/dot-origin".to_string(),
    };
    let snippet = format!(
        "{}{}",
        shell_preamble(&shell_side, ui_total, quiet, verbose),
        format_args!(
            "_pull_overlay {} {} {} {}; code=$?; printf 'rc=%s\\nstatus=%s\\n' \"$code\" \"$REPLY_STATUS\"; {}",
            sq("wname"),
            sq(&shell_side.overlay_text),
            sq(&url_text),
            if optional { "true" } else { "false" },
            shell_probe(&shell_side, tag),
        ),
    );
    let (code, out, err) = shell_run(&shell_side.home, &snippet);
    assert_eq!(code, 0, "harness exit for {tag}");
    let shell_out = normalize(
        &String::from_utf8(out).expect("shell dump"),
        &shell_side.home_text,
        &shell_side.origin_text,
    );
    let shell_err = normalize(
        &String::from_utf8(err).expect("shell warnings"),
        &shell_side.home_text,
        &shell_side.origin_text,
    );

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
    let candidate = candidate_env(&rust_side);
    let logger = Log::new(false, false);
    let palette = plain_palette();
    let empty: &[OsString] = &[];
    let rust_url = match url {
        Url::Missing => String::new(),
        Url::Origin => rust_side.origin_text.clone(),
        Url::Bad => "/nonexistent/dot-origin".to_string(),
    };
    let quiet_text = if quiet { "1" } else { "0" };
    let verbose_text = if verbose { "1" } else { "0" };
    let inputs = PullOverlayInputs {
        name: "wname",
        path: &rust_side.overlay_text,
        url: &rust_url,
        optional,
        extra_args: empty,
        home: &home_text,
        ui_total,
        dot_quiet: Some(quiet_text),
        dot_verbose: Some(verbose_text),
        palette: &palette,
        live_active: false,
        multibyte: false,
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
    let mut stdout = Vec::new();
    let mut warnings = Vec::new();
    let outcome = pull_overlay(&inputs, &mut moves, &mut stdout, &mut warnings);
    // Stdout carries headers and counted-UI rows ahead of the rc
    // lines on both sides; stderr carries the warnings.
    let mut rust_stdout = String::from_utf8(stdout).expect("rust stdout");
    rust_stdout.push_str(&format!(
        "rc={}\nstatus={}\n",
        outcome.rc,
        outcome.status.as_str()
    ));
    rust_stdout.push_str(&rust_probe(&rust_side, tag));
    let rust_stdout = normalize(&rust_stdout, &rust_side.home_text, &rust_side.origin_text);
    let rust_err = normalize(
        &String::from_utf8(warnings).expect("rust warnings"),
        &rust_side.home_text,
        &rust_side.origin_text,
    );
    assert_eq!(rust_stdout, shell_out, "pull stdout for {tag}");
    assert_eq!(rust_err, shell_err, "pull stderr for {tag}");
    assert_eq!(
        outcome.status.as_str(),
        want_status,
        "pull status for {tag}"
    );
    assert_eq!(outcome.rc, 0, "pull rc for {tag}");
}

#[test]
fn pull_overlay_rows_agree() {
    // (tag, optional, ui_total, quiet, verbose, url, want status)
    for (tag, optional, ui_total, quiet, verbose, url, want_status) in [
        (
            "missing-url",
            false,
            None,
            false,
            false,
            Url::Missing,
            "failed",
        ),
        (
            "missing-url-optional",
            true,
            None,
            false,
            false,
            Url::Missing,
            "",
        ),
        ("clone", false, None, false, false, Url::Origin, "cloned"),
        (
            "clone-optional",
            true,
            None,
            false,
            false,
            Url::Origin,
            "cloned",
        ),
        (
            "clone-ui",
            false,
            Some("1"),
            false,
            true,
            Url::Origin,
            "cloned",
        ),
        ("clone-fail", false, None, false, false, Url::Bad, "failed"),
        (
            "clone-fail-optional",
            true,
            None,
            false,
            false,
            Url::Bad,
            "",
        ),
        (
            "not-worktree",
            false,
            None,
            false,
            false,
            Url::Origin,
            "failed",
        ),
        (
            "origin-mismatch",
            false,
            None,
            false,
            false,
            Url::Origin,
            "failed",
        ),
        (
            "origin-mismatch-ui",
            false,
            Some("1"),
            false,
            false,
            Url::Origin,
            "failed",
        ),
        ("skipped", false, None, false, false, Url::Origin, "skipped"),
        (
            "skipped-ui",
            false,
            Some("1"),
            false,
            true,
            Url::Origin,
            "skipped",
        ),
        ("current", false, None, false, true, Url::Origin, "current"),
        (
            "current-ui",
            false,
            Some("1"),
            false,
            true,
            Url::Origin,
            "current",
        ),
        ("changed", false, None, false, false, Url::Origin, "changed"),
        (
            "optional-changed",
            true,
            None,
            false,
            false,
            Url::Origin,
            "changed",
        ),
        (
            "optional-current",
            true,
            None,
            false,
            false,
            Url::Origin,
            "current",
        ),
        (
            "conflict-backup",
            false,
            None,
            false,
            false,
            Url::Origin,
            "changed",
        ),
        ("diverged", false, None, false, false, Url::Origin, "failed"),
        (
            "invalid-candidate",
            false,
            None,
            false,
            false,
            Url::Origin,
            "failed",
        ),
    ] {
        check_row(tag, optional, ui_total, quiet, verbose, url, want_status);
    }
}
