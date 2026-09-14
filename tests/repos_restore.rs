//! Differential parity tests for `restore_installed_links`
//! (`lib/dot/repos/overlays.sh`) against the live shell: the
//! installed-link recovery walk over the rollback snapshot arrays,
//! incl. skip-worktree marking, fallback publication, and the
//! failure leaves.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_base::{Base, Topology};
use dot::repos_overlays::{self, DestinationInputs, RestoreInstalledInputs};
use dot::test_support::TempDir;

/// Sources for the restore walk: overlays plus the model, temp,
/// logging, reservation, and XDG runtime it calls into.
const SOURCES: &str = concat!(
    "dot_xdg_path() { return 1; }\n",
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
    ". \"$1/lib/dot/repos/overlays.sh\"\n",
    ". \"$1/lib/dot/reserved.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
);

/// Run one shell snippet with the restore runtime sourced and the
/// base topology pinned to an ordinary HOME checkout.
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
        .env("DOT_BASE_TOPOLOGY", "ordinary")
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

/// Run `git -C dir args`, silenced, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", dir.display());
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

/// Destination shape probe shared by both sides.
fn dst_state(path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Err(_) => "absent".to_string(),
        Ok(meta) if meta.file_type().is_symlink() => format!(
            "link:{}",
            std::fs::read_link(path)
                .map(|link| link.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        Ok(meta) if meta.is_dir() => "dir".to_string(),
        Ok(meta) if meta.is_file() => format!(
            "file:{}",
            std::fs::read_to_string(path)
                .unwrap_or_default()
                .trim_end_matches('\n')
        ),
        Ok(_) => "other".to_string(),
    }
}

/// Skip-worktree flag for `rel` in the base repo at `home`.
fn skip_flag(home: &Path, rel: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(home)
        .args(["ls-files", "-v", "--", rel])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    let text = String::from_utf8_lossy(&output.stdout);
    if text.starts_with("S ") {
        "skip".to_string()
    } else if text.starts_with("H ") {
        "keep".to_string()
    } else {
        "none".to_string()
    }
}

/// One twin side: an ordinary base checkout at `$HOME` with the
/// rollback arrays, overlay records, and manifest paths.
struct Side {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    manifest: String,
    legacy: String,
}

impl Side {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("fixture home");
        git(&home, &["init", "-q"]);
        let home_text = home.to_string_lossy().into_owned();
        Side {
            _dir: dir,
            home,
            home_text: home_text.clone(),
            manifest: format!("{home_text}/manifest.tsv"),
            legacy: format!("{home_text}/legacy.tsv"),
        }
    }

    fn home_text(&self) -> &str {
        &self.home_text
    }

    /// Commit `rel` with `body` in the base repo.
    fn track(&self, rel: &str, body: &[u8]) {
        stage(&self.home, rel, body);
        git(&self.home, &["add", "--", rel]);
        git(
            &self.home,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "track",
            ],
        );
    }

    /// Shell preamble: rollback arrays, overlay records, manifests.
    fn preamble(&self, rels: &[&str], targets: &[&str], overlays: &[String]) -> String {
        let mut out = format!("export HOME={} ", sq(self.home_text()));
        out.push_str("DOT_OVERLAY_ROLLBACK_PATHS=(");
        for rel in rels {
            out.push_str(&sq(rel));
            out.push(' ');
        }
        out.push_str("); DOT_OVERLAY_ROLLBACK_TARGETS=(");
        for target in targets {
            out.push_str(&sq(target));
            out.push(' ');
        }
        out.push_str("); OVERLAYS=(");
        for entry in overlays {
            out.push_str(&sq(entry));
            out.push(' ');
        }
        // The restore walk reads the activated subset; keep both
        // globals aligned like a converged runtime.
        out.push_str("); ACTIVE_OVERLAYS=(");
        for entry in overlays {
            out.push_str(&sq(entry));
            out.push(' ');
        }
        out.push_str(&format!(
            "); DOT_OVERLAY_MANIFEST={} DOT_OVERLAY_LEGACY_MANIFEST={}; ",
            sq(&self.manifest),
            sq(&self.legacy),
        ));
        out
    }

    /// Aftermath dump appended to the shell snippet, one `d`/`s`
    /// pair per rollback path.
    fn probe(&self, rels: &[&str]) -> String {
        let mut out = String::new();
        for rel in rels {
            out.push_str(&format!(
                "d=absent; dst={}; if [[ -L \"$dst\" ]]; then d=\"link:$(readlink \"$dst\")\"; elif [[ -d \"$dst\" ]]; then d=dir; elif [[ -f \"$dst\" ]]; then d=\"file:$(cat \"$dst\")\"; elif [[ -e \"$dst\" ]]; then d=other; fi; s=none; v=$(git -C {} ls-files -v -- {} 2>/dev/null); case \"$v\" in 'S '*) s=skip;; 'H '*) s=keep;; esac; printf 'd=%s\\ns=%s\\n' \"$d\" \"$s\"; ",
                sq(&format!("{}/{}", self.home_text(), rel)),
                sq(self.home_text()),
                sq(rel),
            ));
        }
        out
    }

    /// Rust inputs mirroring the shell preamble.
    fn inputs<'a>(
        &'a self,
        base: &'a Base,
        dest: &'a DestinationInputs,
        rels: &'a [String],
        targets: &'a [String],
        overlays: &'a [String],
        tool: &'a dot::temp::MoveTool,
    ) -> RestoreInstalledInputs<'a> {
        RestoreInstalledInputs {
            base,
            home: self.home_text(),
            rels,
            targets,
            overlays,
            dest,
            manifest: &self.manifest,
            legacy_manifest: &self.legacy,
            euid: dot::temp::current_uid().expect("uid"),
            source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
            tmp: &self.home,
            tool,
        }
    }
}

/// Base checkout plus destination inputs for one side, with the
/// overlay link paths extracted from the active records.
fn base_and_dest(side: &Side, overlays: &[String]) -> (Base, DestinationInputs) {
    let home = side.home_text().to_owned();
    let overlay_paths: Vec<String> = overlays
        .iter()
        .map(|entry| dot::repos_base::overlay_path_sync(entry).0)
        .collect();
    (
        Base {
            topology: Topology::Ordinary,
            client_git_dir: String::new(),
            home: home.clone(),
        },
        DestinationInputs {
            pwd: home.clone(),
            home,
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            overlay_paths,
            init_backup: None,
        },
    )
}

/// Pending authority presence on one side.
fn pending_state(side: &Side) -> String {
    if std::fs::symlink_metadata(format!("{}.pending", side.manifest)).is_ok() {
        "present".to_string()
    } else {
        "missing".to_string()
    }
}

/// Run one row on twin sides and compare rc plus aftermath. The
/// target builder runs per side because absolute targets embed the
/// side's home directory.
#[allow(clippy::too_many_arguments)]
fn check_row(
    tag: &str,
    rels: &[&str],
    targets: &dyn Fn(&Side) -> Vec<String>,
    overlays: &dyn Fn(&Side) -> Vec<String>,
    setup: &dyn Fn(&Side),
    want_ok: bool,
) {
    let shell_side = Side::build(&format!("{tag}-shell"));
    let rust_side = Side::build(&format!("{tag}-rust"));
    setup(&shell_side);
    setup(&rust_side);
    let shell_targets = targets(&shell_side);
    let shell_overlays = overlays(&shell_side);
    let shell_target_refs: Vec<&str> = shell_targets.iter().map(String::as_str).collect();
    // Pin the topology after sourcing: model.sh detection runs at
    // load and would otherwise report `missing` for the bare
    // fixture checkout. The Rust side binds Ordinary directly, so
    // pin the shell to match.
    let snippet = format!(
        "DOT_BASE_TOPOLOGY=ordinary; {}if _overlay_restore_installed_links; then echo rc=0; else echo rc=1; fi\n{}",
        shell_side.preamble(rels, &shell_target_refs, &shell_overlays),
        shell_side.probe(rels),
    );
    let (status, out, err) = shell_run(&shell_side.home, &snippet);
    assert_eq!(
        status, 0,
        "harness exit for {tag}: stderr={err:?} snippet={snippet:?}"
    );
    assert!(err.is_empty(), "shell stderr for {tag}: {err:?}");
    let shell_dump = String::from_utf8(out).expect("utf8");
    assert!(
        shell_dump.starts_with(if want_ok { "rc=0\n" } else { "rc=1\n" }),
        "shell verdict for {tag}: {shell_dump:?}"
    );
    let targets_owned = targets(&rust_side);
    let overlays_owned = overlays(&rust_side);
    let (base, dest) = base_and_dest(&rust_side, &overlays_owned);
    let rels_owned: Vec<String> = rels.iter().map(|rel| rel.to_string()).collect();
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let inputs = rust_side.inputs(
        &base,
        &dest,
        &rels_owned,
        &targets_owned,
        &overlays_owned,
        &tool,
    );
    let ok = repos_overlays::restore_installed_links(&inputs);
    // Absolute link targets embed the side's home directory;
    // normalize both before comparing.
    let shell_dump = shell_dump.replace(shell_side.home_text(), "@HOME");
    let mut rust_dump = format!("rc={}\n", if ok { 0 } else { 1 });
    for rel in rels {
        rust_dump.push_str(&format!(
            "d={}\ns={}\n",
            dst_state(&rust_side.home.join(*rel)),
            skip_flag(&rust_side.home, rel),
        ));
    }
    let rust_dump = rust_dump.replace(rust_side.home_text(), "@HOME");
    assert_eq!(rust_dump, shell_dump, "twin parity for {tag}");
    assert_eq!(
        pending_state(&rust_side),
        pending_state(&shell_side),
        "pending parity for {tag}"
    );
}

/// Correct link at an available relative target, tracked: rc 0
/// with the skip-worktree bit set.
fn tracked_link_setup(side: &Side) {
    stage(&side.home, "real.txt", b"real\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink("real.txt", side.home.join("owned.txt")).expect("link");
    git(&side.home, &["add", "--", "real.txt", "owned.txt"]);
    git(
        &side.home,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "track",
        ],
    );
}

#[test]
fn restore_rejects_length_mismatch() {
    check_row(
        "length",
        &["a"],
        &|_| vec!["x".to_string(), "y".to_string()],
        &|_| vec![],
        &|_| {},
        false,
    );
}

#[test]
fn restore_keeps_correct_tracked_link() {
    check_row(
        "tracked-link",
        &["owned.txt"],
        &|_| vec!["real.txt".to_string()],
        &|_| vec![],
        &tracked_link_setup,
        true,
    );
}

#[test]
fn restore_keeps_correct_untracked_link() {
    check_row(
        "untracked-link",
        &["owned.txt"],
        &|_| vec!["real.txt".to_string()],
        &|_| vec![],
        &|side| {
            stage(&side.home, "real.txt", b"real\n");
            #[cfg(unix)]
            std::os::unix::fs::symlink("real.txt", side.home.join("owned.txt")).expect("link");
        },
        true,
    );
}

#[test]
fn restore_publishes_missing_tracked_link() {
    check_row(
        "missing-tracked",
        &["owned.txt"],
        &|side| vec![side.home.join("real.txt").to_string_lossy().into_owned()],
        &|_| vec![],
        &|side| {
            let body = b"real\n";
            stage(&side.home, "real.txt", body);
            side.track("owned.txt", body);
            std::fs::remove_file(side.home.join("owned.txt")).expect("remove dst");
        },
        true,
    );
}

#[test]
fn restore_rejects_directory_destination() {
    check_row(
        "dir-blocks",
        &["owned.txt"],
        &|side| vec![side.home.join("real.txt").to_string_lossy().into_owned()],
        &|_| vec![],
        &|side| {
            stage(&side.home, "real.txt", b"real\n");
            std::fs::create_dir_all(side.home.join("owned.txt")).expect("dir dst");
        },
        false,
    );
}

/// Overlay checkout shipping `home/owned.txt` for fallback rows.
fn fallback_overlays(side: &Side) -> Vec<String> {
    let checkout = side.home.join("overlay");
    stage(&checkout, "home/owned.txt", b"shipped\n");
    vec![format!(
        "o|{}|https://example.invalid/x|git||git",
        checkout.to_string_lossy()
    )]
}

#[test]
fn restore_publishes_fallback_link() {
    check_row(
        "fallback",
        &["owned.txt"],
        &|_| vec!["/nonexistent/target".to_string()],
        &fallback_overlays,
        &|_| {},
        true,
    );
}

#[test]
fn restore_keeps_clean_tracked_file_without_fallback() {
    check_row(
        "clean-file",
        &["owned.txt"],
        &|_| vec!["/nonexistent/target".to_string()],
        &|_| vec![],
        &|side| {
            side.track("owned.txt", b"keep\n");
        },
        true,
    );
}

#[test]
fn restore_rejects_wrong_link_without_fallback() {
    check_row(
        "wrong-link",
        &["owned.txt"],
        &|_| vec!["/nonexistent/target".to_string()],
        &|_| vec![],
        &|side| {
            #[cfg(unix)]
            std::os::unix::fs::symlink("/elsewhere", side.home.join("owned.txt")).expect("link");
        },
        false,
    );
}

#[test]
fn restore_republishes_dangling_lost_link_with_fallback() {
    // The live link still names the lost target while a fallback
    // ships: the link's own fingerprint pins the replacement, so
    // the fallback publishes on both sides.
    check_row(
        "lost-link",
        &["owned.txt"],
        &|_| vec!["elsewhere.txt".to_string()],
        &fallback_overlays,
        &|side| {
            #[cfg(unix)]
            std::os::unix::fs::symlink("elsewhere.txt", side.home.join("owned.txt")).expect("link");
        },
        true,
    );
}

#[test]
fn restore_takes_available_fallback_fast_path() {
    // A none-synced overlay's absolute target is available, so a
    // live link to it confirms in place.
    check_row(
        "fallback-fast",
        &["owned.txt"],
        &|_| vec!["/nonexistent/target".to_string()],
        &|side| {
            let checkout = side.home.join("overlay");
            stage(&checkout, "home/owned.txt", b"shipped\n");
            vec![format!(
                "o|{}|https://example.invalid/x|git||none",
                checkout.to_string_lossy()
            )]
        },
        &|side| {
            let shipped = side.home.join("overlay/home/owned.txt");
            #[cfg(unix)]
            std::os::unix::fs::symlink(&shipped, side.home.join("owned.txt")).expect("link");
        },
        true,
    );
}

#[test]
fn restore_applies_good_records_despite_bad_ones() {
    // The walk is sticky: an early publication stands even when a
    // later record fails.
    check_row(
        "sticky",
        &["fresh.txt", "blocked.txt"],
        &|side| {
            vec![
                side.home.join("real.txt").to_string_lossy().into_owned(),
                side.home.join("real.txt").to_string_lossy().into_owned(),
            ]
        },
        &|_| vec![],
        &|side| {
            stage(&side.home, "real.txt", b"real\n");
            std::fs::create_dir_all(side.home.join("blocked.txt")).expect("dir dst");
        },
        false,
    );
}
