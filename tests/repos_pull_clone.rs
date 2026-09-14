//! Differential parity tests for the staged-clone family
//! (`lib/dot/repos/pull.sh`) against the live shell: cloned path
//! modes, cloned mode normalization, the matches-commit gate, and
//! the staged clone orchestrator.
//!
//! Separate binary because the rows build real git fixtures: mode
//! bits and object ids are computed per side, then normalized
//! before comparing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::log::Log;
use dot::repos_pull_clone::{
    CloneOverlayInputs, clone_overlay_staged, cloned_overlay_matches_commit,
    cloned_overlay_path_modes, normalize_cloned_overlay_modes,
};
use dot::repos_pull_queries::CandidateEnv;
use dot::test_support::TempDir;

/// Sources for the staged-clone chapter.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/log.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
    ". \"$1/lib/dot/repos/model.sh\" 2>/dev/null\n",
    ". \"$1/lib/dot/repos/overlays.sh\"\n",
    ". \"$1/lib/dot/reserved.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/repos/pull.sh\"\n",
);

/// Run one shell snippet with the staged-clone runtime sourced.
fn shell_run(home: &Path, snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let home_text = home.to_string_lossy().into_owned();
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("PATH", &path)
        .env("TMPDIR", &tmpdir)
        .env("HOME", home)
        .env("DOT_TEST", "1")
        .env("DOT_SOURCE_ROOT", repo)
        .env("XDG_STATE_HOME", format!("{home_text}/.local/state"))
        .env("SHDEPS_INSTALL_DIR", format!("{home_text}/.local/share"))
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // No pinned locale: forked tools with locale-sensitive
    // diagnostics (`mkdir`, `git`) must speak the same ambient
    // locale on both engines, so pass it through. The fixtures
    // are ASCII, so parsing stays deterministic. This runs after
    // `env_clear`, which wipes everything set before it.
    for (key, value) in locale_passthrough() {
        cmd.env(key, value);
    }
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

/// Capture one git stdout line for fixtures.
fn git_line(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string()
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

/// Ambient locale variables passed to the shell so both engines'
/// forked tools diagnose alike.
fn locale_passthrough() -> Vec<(String, String)> {
    ["LANG", "LC_ALL", "LC_MESSAGES", "LC_CTYPE", "LANGUAGE"]
        .into_iter()
        .filter_map(|key| {
            std::env::var_os(key)
                .map(|value| (key.to_string(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

/// Live umask for the mode ceiling, like the shell reads.
fn mask() -> u32 {
    dot::temp::read_umask().expect("read umask")
}

/// `stat` mode bits, portable across GNU and BSD stat.
fn mode_probe() -> &'static str {
    "m=$(stat -c '%a' \"$p\" 2>/dev/null || stat -f '%Lp' \"$p\" 2>/dev/null || echo NONE); printf 'mode=%s\\n' \"$m\"; "
}

/// Shell preamble: home, empty records, and the topology pin.
fn preamble(home: &str) -> String {
    format!(
        "export HOME={h}; OVERLAYS=(); ACTIVE_OVERLAYS=(); DOT_BASE_TOPOLOGY=ordinary; ",
        h = sq(home),
    )
}

/// One twin side for the direct-function rows: a repo with a
/// committed file, link, and nested file.
struct RepoSide {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    repo: PathBuf,
    repo_text: String,
}

impl RepoSide {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&home).expect("home dir");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git(&repo, &["init", "-q"]);
        stage(&repo, "file.txt", b"data\n");
        stage(&repo, "sub/nested.txt", b"nested\n");
        std::os::unix::fs::symlink("file.txt", repo.join("link")).expect("link");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "seed"]);
        RepoSide {
            _dir: dir,
            home_text: home.to_string_lossy().into_owned(),
            home,
            repo_text: repo.to_string_lossy().into_owned(),
            repo,
        }
    }
}

/// Blob oid of the seeded file.
fn file_oid(repo: &Path) -> String {
    git_line(repo, &["hash-object", "--no-filters", "--", "file.txt"])
}

/// Link-target oid via piped stdin, like the shell feeds it.
fn link_oid(repo: &Path) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hash-object");
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"file.txt")
        .expect("write target");
    let output = child.wait_with_output().expect("hash-object");
    assert!(output.status.success(), "hash link target");
    String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string()
}

/// (rel, mode, oid-shape, setup, want ok, probe mode bits)
/// for the path-modes rows. Oid shapes resolve per side.
#[derive(Clone, Copy)]
enum Oid {
    File,
    Link,
    Bogus,
}

struct ModesRow {
    tag: &'static str,
    rel: &'static str,
    mode: &'static str,
    oid: Oid,
    setup: fn(&RepoSide),
    want_ok: bool,
    probe_mode: bool,
}

fn setup_clean(_side: &RepoSide) {}

fn setup_dirty(side: &RepoSide) {
    stage(&side.repo, "file.txt", b"dirty\n");
}

fn modes_rows() -> Vec<ModesRow> {
    vec![
        ModesRow {
            tag: "file-ok",
            rel: "file.txt",
            mode: "100644",
            oid: Oid::File,
            setup: setup_clean,
            want_ok: true,
            probe_mode: true,
        },
        ModesRow {
            tag: "file-dirty",
            rel: "file.txt",
            mode: "100644",
            oid: Oid::File,
            setup: setup_dirty,
            want_ok: false,
            probe_mode: false,
        },
        ModesRow {
            tag: "link-ok",
            rel: "link",
            mode: "120000",
            oid: Oid::Link,
            setup: setup_clean,
            want_ok: true,
            probe_mode: false,
        },
        ModesRow {
            tag: "link-wrong",
            rel: "link",
            mode: "120000",
            oid: Oid::Bogus,
            setup: setup_clean,
            want_ok: false,
            probe_mode: false,
        },
        ModesRow {
            tag: "missing",
            rel: "gone.txt",
            mode: "100644",
            oid: Oid::Bogus,
            setup: setup_clean,
            want_ok: false,
            probe_mode: false,
        },
        ModesRow {
            tag: "dir-as-file",
            rel: "sub",
            mode: "100644",
            oid: Oid::Bogus,
            setup: setup_clean,
            want_ok: false,
            probe_mode: false,
        },
        ModesRow {
            tag: "bad-relative",
            rel: "../evil",
            mode: "100644",
            oid: Oid::Bogus,
            setup: setup_clean,
            want_ok: false,
            probe_mode: false,
        },
        ModesRow {
            tag: "nested-ok",
            rel: "sub/nested.txt",
            mode: "100644",
            oid: Oid::Bogus,
            setup: setup_clean,
            want_ok: true,
            probe_mode: false,
        },
    ]
}

/// Resolve the oid shape per side (the nested oid comes from its
/// own blob so the row passes).
fn resolve_oid(repo: &Path, shape: Oid, nested: &str) -> String {
    match shape {
        Oid::File => file_oid(repo),
        Oid::Link => link_oid(repo),
        Oid::Bogus => nested.to_string(),
    }
}

/// Portable mode-bits probe for one path.
fn rust_mode(path: &Path) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    match std::fs::metadata(path) {
        Ok(meta) => format!("{:o}", meta.permissions().mode() & 0o777),
        Err(_) => "NONE".to_string(),
    }
}

#[test]
fn cloned_path_modes_agree() {
    for row in modes_rows() {
        let shell_side = RepoSide::build(&format!("{}-shell", row.tag));
        let rust_side = RepoSide::build(&format!("{}-rust", row.tag));
        // Oids resolve on the clean tree: setup may dirty the
        // worktree afterwards, which is exactly what the false
        // rows detect.
        let nested_oid_shell = git_line(
            &shell_side.repo,
            &["hash-object", "--no-filters", "--", "sub/nested.txt"],
        );
        let nested_oid_rust = git_line(
            &rust_side.repo,
            &["hash-object", "--no-filters", "--", "sub/nested.txt"],
        );
        let shell_oid = resolve_oid(&shell_side.repo, row.oid, &nested_oid_shell);
        let rust_oid = resolve_oid(&rust_side.repo, row.oid, &nested_oid_rust);
        (row.setup)(&shell_side);
        (row.setup)(&rust_side);
        let mut snippet = format!(
            "{}p={}; if _repo_cloned_overlay_path_modes {} {} {} {}; then echo rc=0; else echo rc=1; fi; ",
            preamble(&shell_side.home_text),
            sq(&format!("{}/{}", shell_side.repo_text, row.rel)),
            sq(&shell_side.repo_text),
            sq(row.rel),
            sq(row.mode),
            sq(&shell_oid),
        );
        if row.probe_mode {
            snippet.push_str(mode_probe());
        }
        let (code, out, err) = shell_run(&shell_side.home, &snippet);
        assert_eq!(code, 0, "harness exit for {}", row.tag);
        assert!(err.is_empty(), "shell stderr for {}: {err:?}", row.tag);
        let shell = String::from_utf8(out).expect("shell dump");

        let rust_ok =
            cloned_overlay_path_modes(&rust_side.repo_text, row.rel, row.mode, &rust_oid, mask());
        let mut rust = format!("rc={}\n", if rust_ok { 0 } else { 1 });
        if row.probe_mode {
            rust.push_str(&format!(
                "mode={}\n",
                rust_mode(&rust_side.repo.join(row.rel))
            ));
        }
        assert_eq!(rust, shell, "path modes for {}", row.tag);
        assert_eq!(rust_ok, row.want_ok, "path modes rc for {}", row.tag);
    }
}

/// Normalize rows: (tag, setup, want ok, probe mode bits).
fn normalize_setup(side: &RepoSide, tag: &str) {
    match tag {
        "clean" | "untracked" => {}
        "dirty" => {
            stage(&side.repo, "file.txt", b"dirty\n");
        }
        "staged" => {
            stage(&side.repo, "file.txt", b"staged\n");
            git(&side.repo, &["add", "--", "file.txt"]);
        }
        "repair" => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(
                    side.repo.join("file.txt"),
                    std::fs::Permissions::from_mode(0o777),
                )
                .expect("chmod fixture");
            }
        }
        _ => unreachable!("unknown normalize row {tag}"),
    }
    if tag == "untracked" {
        stage(&side.repo, "extra.txt", b"untracked\n");
    }
}

#[test]
fn normalize_cloned_modes_agree() {
    for (tag, want_ok, probe_mode) in [
        ("clean", true, false),
        ("dirty", false, false),
        ("staged", false, false),
        ("untracked", true, false),
        ("repair", true, true),
    ] {
        let shell_side = RepoSide::build(&format!("{tag}-shell"));
        let rust_side = RepoSide::build(&format!("{tag}-rust"));
        normalize_setup(&shell_side, tag);
        normalize_setup(&rust_side, tag);
        let shell_commit = git_line(&shell_side.repo, &["rev-parse", "HEAD"]);
        let rust_commit = git_line(&rust_side.repo, &["rev-parse", "HEAD"]);
        let mut snippet = format!(
            "{}p={}; if _repo_normalize_cloned_overlay_modes {} {}; then echo rc=0; else echo rc=1; fi; ",
            preamble(&shell_side.home_text),
            sq(&shell_side.repo.join("file.txt").to_string_lossy()),
            sq(&shell_side.repo_text),
            sq(&shell_commit),
        );
        if probe_mode {
            snippet.push_str(mode_probe());
        }
        let (code, out, err) = shell_run(&shell_side.home, &snippet);
        assert_eq!(code, 0, "harness exit for {tag}");
        assert!(err.is_empty(), "shell stderr for {tag}: {err:?}");
        let shell = String::from_utf8(out).expect("shell dump");

        let rust_ok = normalize_cloned_overlay_modes(&rust_side.repo_text, &rust_commit, mask());
        let mut rust = format!("rc={}\n", if rust_ok { 0 } else { 1 });
        if probe_mode {
            rust.push_str(&format!(
                "mode={}\n",
                rust_mode(&rust_side.repo.join("file.txt"))
            ));
        }
        assert_eq!(rust, shell, "normalize modes for {tag}");
        assert_eq!(rust_ok, want_ok, "normalize modes rc for {tag}");
    }
}

#[test]
fn cloned_matches_commit_agrees() {
    for (tag, want_ok) in [
        ("clean", true),
        ("dirty", false),
        ("staged", false),
        ("untracked", false),
    ] {
        let shell_side = RepoSide::build(&format!("match-{tag}-shell"));
        let rust_side = RepoSide::build(&format!("match-{tag}-rust"));
        normalize_setup(&shell_side, tag);
        normalize_setup(&rust_side, tag);
        let shell_commit = git_line(&shell_side.repo, &["rev-parse", "HEAD"]);
        let rust_commit = git_line(&rust_side.repo, &["rev-parse", "HEAD"]);
        let snippet = format!(
            "{}if _repo_cloned_overlay_matches_commit {} {}; then echo rc=0; else echo rc=1; fi\n",
            preamble(&shell_side.home_text),
            sq(&shell_side.repo_text),
            sq(&shell_commit),
        );
        let (code, out, err) = shell_run(&shell_side.home, &snippet);
        assert_eq!(code, 0, "harness exit for match-{tag}");
        assert!(err.is_empty(), "shell stderr for match-{tag}: {err:?}");
        let shell = String::from_utf8(out).expect("shell dump");

        let rust_ok = cloned_overlay_matches_commit(&rust_side.repo_text, &rust_commit);
        assert_eq!(
            format!("rc={}\n", if rust_ok { 0 } else { 1 }),
            shell,
            "matches commit for match-{tag}"
        );
        assert_eq!(rust_ok, want_ok, "matches commit rc for match-{tag}");
    }
}

/// One twin side for the full clone: an origin plus a parent dir
/// receiving the clone.
struct CloneSide {
    _dir: TempDir,
    home: PathBuf,
    home_text: String,
    origin_text: String,
    parent: PathBuf,
    parent_text: String,
    dest: PathBuf,
    dest_text: String,
}

impl CloneSide {
    fn build(tag: &str, evil: bool) -> Self {
        let dir = TempDir::new(tag).expect("fixture dir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("home dir");
        let origin = dir.path().join("origin");
        std::fs::create_dir_all(&origin).expect("origin dir");
        git(&origin, &["init", "-q"]);
        stage(&origin, "base.txt", b"origin\n");
        if evil {
            // Overlay validation only polices home/-rooted
            // paths; a reserved leaf there rejects the tree.
            stage(&origin, "home/.dotfiles/evil", b"x\n");
        }
        git(&origin, &["add", "-A"]);
        git(&origin, &["commit", "-qm", "seed"]);
        // The parent is left for each row: most rows let the clone
        // create it via `mkdir -p`, while `dest-exists` and `bad-url`
        // need it up front and `no-parent` blocks it with a file.
        let parent = dir.path().join("parent");
        let dest = parent.join("checkout");
        CloneSide {
            _dir: dir,
            home_text: home.to_string_lossy().into_owned(),
            home,
            origin_text: origin.to_string_lossy().into_owned(),
            parent_text: parent.to_string_lossy().into_owned(),
            parent,
            dest_text: dest.to_string_lossy().into_owned(),
            dest,
        }
    }
}

/// Candidate environment mirroring the shell preamble.
fn candidate_env(home: &str) -> CandidateEnv {
    CandidateEnv {
        home: home.to_string(),
        checkout: format!("{home}/.local/share/cgraf78/dot"),
        pwd: home.to_string(),
        source_root: env!("CARGO_MANIFEST_DIR").to_string(),
        state_home: format!("{home}/.local/state"),
        install_root: format!("{home}/.local/share"),
        provider_state: format!("{home}/.local/state/shdeps"),
        overlay_paths: Vec::new(),
        init_backup: None,
    }
}

/// Shell aftermath probe for a clone: destination state, recorded
/// origin URL, and stage-directory leaks in the parent.
fn clone_probe(side: &CloneSide) -> String {
    format!(
        "d=absent; dst={}; if [[ -d \"$dst/.git\" ]]; then d=\"worktree:$(cat \"$dst/base.txt\" 2>/dev/null || echo NOREAD)\"; elif [[ -e \"$dst\" || -L \"$dst\" ]]; then d=other; fi; \
         u=$(git -C \"$dst\" config --get remote.origin.url 2>/dev/null || echo NOURL); \
         leaked=no; for e in {}/.*.clone.*; do [[ -e \"$e\" ]] && leaked=yes; done; \
         printf 'd=%s\\nu=%s\\nleaked=%s\\n' \"$d\" \"$u\" \"$leaked\"\n",
        sq(&side.dest_text),
        sq(&side.parent_text),
    )
}

/// Rust aftermath mirroring [`clone_probe`].
fn clone_rust(side: &CloneSide) -> String {
    let dest = &side.dest;
    let state = if dest.join(".git").is_dir() {
        let content =
            std::fs::read_to_string(dest.join("base.txt")).unwrap_or_else(|_| "NOREAD".to_string());
        format!("worktree:{}", content.trim_end_matches('\n'))
    } else if std::fs::symlink_metadata(dest).is_ok() {
        "other".to_string()
    } else {
        "absent".to_string()
    };
    let url = git_line_opt(dest);
    let leaked = std::fs::read_dir(&side.parent).is_ok_and(|entries| {
        entries
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.file_name().to_string_lossy().contains(".clone."))
    });
    format!(
        "d={state}\nu={url}\nleaked={}\n",
        if leaked { "yes" } else { "no" }
    )
}

/// Origin URL or `NOURL` when unreadable, like the shell probe.
fn git_line_opt(repo: &Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["config", "--get", "remote.origin.url"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string(),
        _ => "NOURL".to_string(),
    }
}

/// Run one clone row on twin sides and compare rc, warnings, and
/// aftermath. `setup` mutates the side past the seed clone inputs
/// (dest-blocker, bad url); the url builder runs per side.
fn check_clone_row(
    tag: &str,
    evil: bool,
    url: &dyn Fn(&CloneSide) -> String,
    setup: &dyn Fn(&CloneSide),
    want_ok: bool,
) {
    let shell_side = CloneSide::build(&format!("{tag}-shell"), evil);
    let rust_side = CloneSide::build(&format!("{tag}-rust"), evil);
    setup(&shell_side);
    setup(&rust_side);
    let shell_url = url(&shell_side);
    let rust_url = url(&rust_side);
    let snippet = format!(
        "{}if _repo_clone_overlay_staged {} {}; then echo rc=0; else echo rc=1; fi; {}",
        preamble(&shell_side.home_text),
        sq(&shell_url),
        sq(&shell_side.dest_text),
        clone_probe(&shell_side),
    );
    let (code, out, err) = shell_run(&shell_side.home, &snippet);
    assert_eq!(code, 0, "harness exit for {tag}");
    let shell = format!(
        "{}{}",
        String::from_utf8(out).expect("shell dump"),
        String::from_utf8(err).expect("shell warnings"),
    );
    // Every compared path (home, parent, origin, dest) lives under
    // the side dir; one replacement covers them all.
    let shell_dir = shell_side._dir.path().to_string_lossy().into_owned();
    let shell = shell.replace(&shell_dir, "@SIDE@");

    let logger = Log::new(false, false);
    let candidate = candidate_env(&rust_side.home_text);
    let inputs = CloneOverlayInputs {
        url: &rust_url,
        path: &rust_side.dest_text,
        candidate: &candidate,
        mask: mask(),
        log: &logger,
    };
    let mut moves = dot::temp::MoveCache::default();
    let mut warnings = Vec::new();
    let rust_ok = clone_overlay_staged(&inputs, &mut moves, &mut warnings);
    let mut rust = format!("rc={}\n", if rust_ok { 0 } else { 1 });
    rust.push_str(&clone_rust(&rust_side));
    rust.push_str(&String::from_utf8(warnings).expect("rust warnings"));
    let rust_dir = rust_side._dir.path().to_string_lossy().into_owned();
    let rust = rust.replace(&rust_dir, "@SIDE@");
    assert_eq!(rust, shell, "clone aftermath for {tag}");
    assert_eq!(rust_ok, want_ok, "clone rc for {tag}");
}

#[test]
fn clone_overlay_staged_agrees() {
    let none = |_: &CloneSide| {};
    let origin_url = |side: &CloneSide| side.origin_text.clone();
    // (tag, evil tree, url, setup, want ok)
    check_clone_row("happy", false, &origin_url, &none, true);
    check_clone_row("invalid-tree", true, &origin_url, &none, false);
    check_clone_row(
        "dest-exists",
        false,
        &origin_url,
        &|side: &CloneSide| {
            stage(&side.parent, "checkout", b"user data\n");
        },
        false,
    );
    check_clone_row(
        "bad-url",
        false,
        &|side: &CloneSide| format!("{}/does-not-exist", side.origin_text),
        &|side: &CloneSide| {
            std::fs::create_dir_all(&side.parent).expect("parent dir");
        },
        false,
    );
    check_clone_row(
        "no-parent",
        false,
        &origin_url,
        &|side: &CloneSide| {
            std::fs::write(&side.parent, b"blocker\n").expect("parent blocker");
        },
        false,
    );
}
