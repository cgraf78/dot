//! Differential parity tests for `_pull_repo` and `_pull_base`
//! (`lib/dot/repos/pull.sh`) against the live shell: the logged
//! pull with conflict-backup retry, and the base orchestrator
//! (upstream check, fast-path acceptance, candidate validation,
//! parent snapshot, pull, and mode normalization).
//!
//! Separate binary because the rows drive real `git pull` runs:
//! each side builds its own origin plus clone under disjoint
//! directories, so paths and hashes normalize before comparing.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::repos_base::{Base, Topology};
use dot::repos_overlays::DestinationInputs;
use dot::repos_pull::{PullBaseInputs, pull_base};
use dot::repos_pull_queries::CandidateEnv;
use dot::test_support::TempDir;

/// Sources for the base-pull chapter.
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
/// stays pinned: `git pull` output must read English on both
/// engines, and the port pins `LC_ALL=C` around every git run like
/// `_pull_cmd` does.
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

/// One twin side: an origin repo plus a clone at `$HOME`.
struct Side {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    origin: PathBuf,
    origin_text: String,
    manifest: String,
    legacy: String,
}

impl Side {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let origin = dir.path().join("origin");
        std::fs::create_dir_all(&origin).expect("origin dir");
        git(&origin, &["init", "-q"]);
        stage(&origin, "base.txt", b"v1\n");
        commit(&origin, "seed");
        let home = dir.path().join("home");
        let status = Command::new("git")
            .arg("clone")
            .arg("-q")
            .arg(&origin)
            .arg(&home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn git clone");
        assert!(status.success(), "clone for {tag}");
        let home_text = home.to_string_lossy().into_owned();
        let origin_text = origin.to_string_lossy().into_owned();
        Side {
            _dir: dir,
            home,
            home_text: home_text.clone(),
            origin,
            origin_text,
            manifest: format!("{home_text}/manifest.tsv"),
            legacy: format!("{home_text}/legacy.tsv"),
        }
    }
}

/// Per-row fixture after the seed clone.
fn setup_side(side: &Side, tag: &str) {
    match tag {
        "skipped" => {
            git(&side.home, &["branch", "--unset-upstream"]);
        }
        "changed" => {
            stage(&side.origin, "newfile.txt", b"from origin\n");
            commit(&side.origin, "add newfile");
        }
        "conflict-backup" => {
            stage(&side.origin, "clash.txt", b"origin clash\n");
            commit(&side.origin, "add clash");
            stage(&side.home, "clash.txt", b"user clash\n");
        }
        "diverged" => {
            stage(&side.home, "base.txt", b"home change\n");
            commit(&side.home, "home change");
            stage(&side.origin, "base.txt", b"origin change\n");
            commit(&side.origin, "origin change");
        }
        "invalid-candidate" => {
            stage(&side.origin, ".dotfiles/evil", b"x\n");
            commit(&side.origin, "add evil");
        }
        "current" | "current-quiet" => {}
        _ => unreachable!("unknown row {tag}"),
    }
}

/// Aftermath probes per row.
fn probe_rels(tag: &str) -> &'static [&'static str] {
    match tag {
        "changed" => &["newfile.txt"],
        "conflict-backup" => &["clash.txt"],
        "invalid-candidate" => &[".dotfiles/evil"],
        _ => &[],
    }
}

/// Shell aftermath probe: one `st=` line per rel plus the backup
/// entry count.
fn shell_probe(side: &Side, tag: &str) -> String {
    let mut out = String::new();
    for rel in probe_rels(tag) {
        out.push_str(&format!(
            "p={}; if [[ -f \"$p\" ]]; then printf 'st={rel}:file:%s\\n' \"$(cat \"$p\")\"; \
             else printf 'st={rel}:absent\\n'; fi; ",
            sq(&format!("{}/{}", side.home_text, rel)),
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

/// Shell preamble: home, quiet/verbose flags, empty rollback and
/// overlay records, manifests, and the candidate-validation
/// environment; the topology pins after sourcing because model.sh
/// detection runs at load.
fn shell_preamble(side: &Side, quiet: bool, verbose: bool) -> String {
    let home_text = &side.home_text;
    format!(
        "export HOME={h} DOT_QUIET={q} DOT_VERBOSE={v} XDG_STATE_HOME={h}/.local/state SHDEPS_INSTALL_DIR={h}/.local/share; \
         DOT_OVERLAY_ROLLBACK_PATHS=(); DOT_OVERLAY_ROLLBACK_TARGETS=(); OVERLAYS=(); ACTIVE_OVERLAYS=(); \
         DOT_OVERLAY_MANIFEST={m} DOT_OVERLAY_LEGACY_MANIFEST={l}; DOT_BASE_TOPOLOGY=ordinary; ",
        h = sq(home_text),
        q = u8::from(quiet),
        v = u8::from(verbose),
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

/// Rust aftermath dump mirroring [`shell_probe`].
fn rust_probe(side: &Side, tag: &str) -> String {
    let mut out = String::new();
    for rel in probe_rels(tag) {
        let path = side.home.join(rel);
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
/// and aftermath.
fn check_row(tag: &str, quiet: bool, verbose: bool, want_status: &str, want_rc: i32) {
    let shell_side = Side::build(&format!("{tag}-shell"));
    let rust_side = Side::build(&format!("{tag}-rust"));
    setup_side(&shell_side, tag);
    setup_side(&rust_side, tag);
    let snippet = format!(
        "{}{}",
        shell_preamble(&shell_side, quiet, verbose),
        format_args!(
            "_pull_base; code=$?; printf 'rc=%s\\nstatus=%s\\n' \"$code\" \"$REPLY_STATUS\"; {}",
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
    let empty: &[OsString] = &[];
    let inputs = PullBaseInputs {
        base: &base,
        candidate: &candidate,
        quarantine: None,
        overlays: &[],
        dest: &dest,
        manifest: &rust_side.manifest,
        legacy_manifest: &rust_side.legacy,
        euid: dot::temp::current_uid().expect("uid"),
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        tmp: &rust_side.home,
        tool: &tool,
        extra_args: empty,
        quiet,
        verbose,
        log: &logger,
    };
    let mut stdout = Vec::new();
    let mut warnings = Vec::new();
    let outcome = pull_base(&inputs, &mut moves, &mut stdout, &mut warnings);
    // Stdout carries the dim log dump ahead of the rc lines on both
    // sides; stderr carries the warnings.
    let mut rust_stdout = String::from_utf8(stdout).expect("rust stdout");
    rust_stdout.push_str(&format!(
        "rc={}\nstatus={}\n",
        outcome.rc,
        outcome.status.as_str(),
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
    assert_eq!(outcome.rc, want_rc, "pull rc for {tag}");
}

#[test]
fn pull_base_rows_agree() {
    // (tag, quiet, verbose, want status, want rc)
    for (tag, quiet, verbose, want_status, want_rc) in [
        ("skipped", false, false, "skipped", 0),
        ("current", false, true, "current", 0),
        ("current-quiet", true, false, "current", 0),
        ("changed", false, false, "changed", 0),
        ("conflict-backup", false, false, "changed", 0),
        ("diverged", false, false, "failed", 1),
        ("invalid-candidate", false, false, "failed", 1),
    ] {
        check_row(tag, quiet, verbose, want_status, want_rc);
    }
}
