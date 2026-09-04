//! Differential parity tests for host Git selection and repository
//! identity (`lib/dot/init-client.sh`, part 2) against the live
//! shell: the pinned host `git` search with its client-root
//! exclusions, the shell-function guard, repository URL
//! normalization, branch-name validation, and remote
//! default-branch resolution.
//!
//! Separate binary because the git rows drive real `clone` /
//! `ls-remote` runs and the selection rows build fixture `PATH`
//! trees: fixtures live under one shared directory so both engines
//! resolve identical absolute paths, while each engine keeps a
//! private scratch for probe stages.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_identity as init;
use dot::test_support::TempDir;

/// Sources for the identity chapter: the cleanup allocator (probe
/// stages for default-branch resolution) and the init client
/// itself. Selection, binding, identity, and branch validation
/// need nothing else; `temp.sh` / `public/xdg.sh` stay out so the
/// harness spells exactly what the family consumes.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Run one shell snippet with the identity runtime sourced. The
/// locale stays pinned: `git` diagnostics must read English on both
/// engines, and the port pins `LC_ALL=C` around every git run like
/// the shell helpers do.
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

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Twin homes: disjoint directories so probe stages never collide
/// across engines. Selection fixtures live under the shared root
/// instead, so both engines resolve byte-identical paths.
struct Twins {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
}

impl Twins {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let shell_home = dir.path().join("sh-home");
        let rust_home = dir.path().join("rs-home");
        std::fs::create_dir_all(&shell_home).expect("shell home");
        std::fs::create_dir_all(&rust_home).expect("rust home");
        Self {
            _dir: dir,
            shell_home,
            rust_home,
        }
    }

    /// Shared fixture root: identical absolute paths for both
    /// engines (selection inputs, origins, file-URL targets).
    fn shared(&self) -> &Path {
        self._dir.path()
    }
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

/// Commit everything with `message`.
fn commit(repo: &Path, message: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", message]);
}

/// Build an origin repo with `branches` (first is the initial
/// branch) each carrying one commit, with `HEAD` pointing at
/// `head` (which may be unborn, to force the fallback paths).
fn make_origin(root: &Path, name: &str, branches: &[&str], head: &str) -> PathBuf {
    let origin = root.join(name);
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg("-b")
        .arg(branches[0])
        .arg(&origin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git init");
    assert!(status.success(), "init {}", origin.display());
    for (idx, branch) in branches.iter().enumerate() {
        if idx > 0 {
            git(&origin, &["branch", branch]);
            git(&origin, &["checkout", "-q", branch]);
        }
        std::fs::write(origin.join(format!("file-{branch}")), branch.as_bytes())
            .expect("fixture file");
        commit(&origin, branch);
    }
    git(
        &origin,
        &["symbolic-ref", "HEAD", &format!("refs/heads/{head}")],
    );
    origin
}

/// Write an executable `git` fixture (never executed by the
/// selection probes — only its file bits matter — but executable
/// so the shape matches a real host tool).
fn fake_git(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).expect("fixture dir");
    let git = dir.join("git");
    std::fs::write(&git, b"#!/bin/sh\necho fake\n").expect("fake git");
    chmod(&git, 0o755);
    git
}

/// `chmod` without following the test's own outcome plumbing.
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// Shell verdict for one selection probe: (exit code, stdout).
fn shell_select(home: &Path, fake_home: &str, source: &str, path: &str) -> (i32, String) {
    let snippet = format!(
        "export HOME={} DOT_SOURCE_ROOT={} PATH={}\nREPLY=; _dot_init_select_host_git; code=$?; printf 'code=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"\n",
        sq(fake_home),
        sq(source),
        sq(path),
    );
    let (code, out, _) = shell_run(home, &snippet);
    (code, String::from_utf8_lossy(&out).into_owned())
}

/// Rust verdict in the same shape.
fn rust_select(fake_home: &str, source: &str, path: &str) -> String {
    match init::select_host_git(fake_home, source, path) {
        Some(reply) => format!("code=0\nreply={reply}\n"),
        None => "code=1\nreply=\n".to_string(),
    }
}

#[test]
fn select_prefers_first_usable_git() {
    let twins = Twins::build("init-ident-select-first");
    let root = twins.shared();
    let fake_home = root.join("home");
    let source = root.join("source");
    std::fs::create_dir_all(&fake_home).expect("fake home");
    std::fs::create_dir_all(&source).expect("fake source");
    let empty = root.join("empty");
    std::fs::create_dir_all(&empty).expect("empty dir");
    let first = root.join("first");
    let second = root.join("second");
    fake_git(&first);
    fake_git(&second);
    let home = fake_home.to_string_lossy().into_owned();
    let source = source.to_string_lossy().into_owned();
    let path = format!(
        "{}:{}:{}",
        empty.display(),
        first.display(),
        second.display()
    );
    let (code, out) = shell_select(&twins.shell_home, &home, &source, &path);
    assert_eq!(code, 0);
    assert_eq!(out, rust_select(&home, &source, &path));
    assert_eq!(out, format!("code=0\nreply={}/git\n", first.display()));
}

#[test]
fn select_skips_unusable_candidates() {
    let twins = Twins::build("init-ident-select-skip");
    let root = twins.shared();
    let fake_home = root.join("home");
    let source = root.join("source");
    std::fs::create_dir_all(&fake_home).expect("fake home");
    std::fs::create_dir_all(&source).expect("fake source");
    // Symlinked executable: `-f` follows but `!-L` rejects.
    let linked = root.join("linked");
    let target = root.join("target");
    fake_git(&target);
    std::fs::create_dir_all(&linked).expect("link dir");
    std::os::unix::fs::symlink(target.join("git"), linked.join("git")).expect("git symlink");
    // Unexecutable regular file.
    let flat = root.join("flat");
    std::fs::create_dir_all(&flat).expect("flat dir");
    std::fs::write(flat.join("git"), b"#!/bin/sh\n").expect("flat git");
    chmod(&flat.join("git"), 0o644);
    // Directory named `git`: not a file.
    let dirgit = root.join("dirgit");
    std::fs::create_dir_all(dirgit.join("git")).expect("dir git");
    // Good candidate last, after a relative entry and a missing
    // absolute directory (both skipped without probing).
    let good = root.join("good");
    fake_git(&good);
    let home = fake_home.to_string_lossy().into_owned();
    let source = source.to_string_lossy().into_owned();
    let path = format!(
        "relative:{}:{}:{}:{}:{}",
        linked.display(),
        flat.display(),
        dirgit.display(),
        root.join("missing").display(),
        good.display(),
    );
    let (code, out) = shell_select(&twins.shell_home, &home, &source, &path);
    assert_eq!(code, 0);
    assert_eq!(out, rust_select(&home, &source, &path));
    assert_eq!(out, format!("code=0\nreply={}/git\n", good.display()));
}

#[test]
fn select_excludes_client_roots() {
    let twins = Twins::build("init-ident-select-excl");
    let root = twins.shared();
    let fake_home = root.join("home");
    let source = root.join("source");
    fake_git(&fake_home.join("hostbin"));
    fake_git(&source.join("srcbin"));
    let outside = root.join("outside");
    fake_git(&outside);
    let home_text = fake_home.to_string_lossy().into_owned();
    let source_text = source.to_string_lossy().into_owned();
    // Home and source candidates lose to the outsider.
    let path = format!(
        "{}/hostbin:{}/srcbin:{}",
        fake_home.display(),
        source.display(),
        outside.display()
    );
    let (code, out) = shell_select(&twins.shell_home, &home_text, &source_text, &path);
    assert_eq!(code, 0);
    assert_eq!(out, rust_select(&home_text, &source_text, &path));
    assert_eq!(out, format!("code=0\nreply={}/git\n", outside.display()));
    // Nothing outside: the exclusions leave no candidate.
    let path = format!(
        "{}/hostbin:{}/srcbin",
        fake_home.display(),
        source.display()
    );
    let (code, out) = shell_select(&twins.shell_home, &home_text, &source_text, &path);
    assert_eq!(code, 0);
    assert_eq!(out, rust_select(&home_text, &source_text, &path));
    assert_eq!(out, "code=1\nreply=\n");
    // A `/` home excludes everything, like the shell's leading
    // `$home_physical == /` disjunct.
    let path = outside.display().to_string();
    let (code, out) = shell_select(&twins.shell_home, "/", &source_text, &path);
    assert_eq!(code, 0);
    assert_eq!(out, rust_select("/", &source_text, &path));
    assert_eq!(out, "code=1\nreply=\n");
}

#[test]
fn select_fails_without_resolvable_roots() {
    let twins = Twins::build("init-ident-select-roots");
    let root = twins.shared();
    let outside = root.join("outside");
    fake_git(&outside);
    let path = outside.display().to_string();
    // Missing home.
    let (code, out) = shell_select(
        &twins.shell_home,
        &root.join("no-home").to_string_lossy(),
        &root.join("no-source").to_string_lossy(),
        &path,
    );
    assert_eq!(code, 0);
    assert_eq!(
        out,
        rust_select(
            &root.join("no-home").to_string_lossy(),
            &root.join("no-source").to_string_lossy(),
            &path
        )
    );
    assert_eq!(out, "code=1\nreply=\n");
    // Empty path: no directories to scan.
    let home = root.join("home");
    let source = root.join("source");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&source).expect("source");
    let home = home.to_string_lossy().into_owned();
    let source = source.to_string_lossy().into_owned();
    let (code, out) = shell_select(&twins.shell_home, &home, &source, "");
    assert_eq!(code, 0);
    assert_eq!(out, rust_select(&home, &source, ""));
    assert_eq!(out, "code=1\nreply=\n");
}

/// Shell verdict for one bind probe: (exit code, stdout, stderr).
/// Success prints the hashed `git`; failures print only the
/// `dot init: ` diagnostic, like `_dot_init_error`.
fn shell_bind(
    home: &Path,
    fake_home: &str,
    source: &str,
    path: &str,
    shadow: bool,
) -> (i32, String, String) {
    let shadow_setup = if shadow { "git() { :; }\n" } else { "" };
    // The trailing `if` (never `&&`) keeps the oracle exit at 0 on
    // failure rows: the verdict travels in the captured streams.
    let snippet = format!(
        "export HOME={} DOT_SOURCE_ROOT={} PATH={}\n{shadow_setup}_dot_init_bind_host_git; code=$?; printf 'code=%s\\n' \"$code\"; if [[ $code -eq 0 ]]; then command -v git; fi\n",
        sq(fake_home),
        sq(source),
        sq(path),
    );
    let (code, out, err) = shell_run(home, &snippet);
    (
        code,
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    )
}

/// Rust verdict in the same shape: the `Err` payload renders with
/// the `dot init: ` prefix the shell's `_dot_init_error` adds.
fn rust_bind(fake_home: &str, source: &str, path: &str, shadowed: bool) -> (String, String) {
    match init::bind_host_git(fake_home, source, path, shadowed) {
        Ok(host) => (format!("code=0\n{host}\n"), String::new()),
        Err(message) => ("code=1\n".to_string(), format!("dot init: {message}\n")),
    }
}

#[test]
fn bind_pins_selected_git() {
    let twins = Twins::build("init-ident-bind-ok");
    let root = twins.shared();
    let fake_home = root.join("home");
    let source = root.join("source");
    std::fs::create_dir_all(&fake_home).expect("fake home");
    std::fs::create_dir_all(&source).expect("fake source");
    let tools = root.join("tools");
    fake_git(&tools);
    let home = fake_home.to_string_lossy().into_owned();
    let source = source.to_string_lossy().into_owned();
    let path = tools.display().to_string();
    let (code, out, err) = shell_bind(&twins.shell_home, &home, &source, &path, false);
    let (rust_out, rust_err) = rust_bind(&home, &source, &path, false);
    assert_eq!(code, 0);
    assert_eq!((&out, &err), (&rust_out, &rust_err));
    assert_eq!(out, format!("code=0\n{}/git\n", tools.display()));
    assert_eq!(err, "");
}

#[test]
fn bind_rejects_missing_git() {
    let twins = Twins::build("init-ident-bind-missing");
    let root = twins.shared();
    let fake_home = root.join("home");
    let source = root.join("source");
    let empty = root.join("empty");
    for dir in [&fake_home, &source, &empty] {
        std::fs::create_dir_all(dir).expect("fixture dir");
    }
    let home = fake_home.to_string_lossy().into_owned();
    let source = source.to_string_lossy().into_owned();
    let path = empty.display().to_string();
    let (code, out, err) = shell_bind(&twins.shell_home, &home, &source, &path, false);
    let (rust_out, rust_err) = rust_bind(&home, &source, &path, false);
    assert_eq!(code, 0);
    assert_eq!((&out, &err), (&rust_out, &rust_err));
    assert_eq!(out, "code=1\n");
    assert_eq!(err, format!("dot init: {}\n", init::NO_HOST_GIT));
}

#[test]
fn bind_rejects_shadowed_git() {
    let twins = Twins::build("init-ident-bind-shadow");
    let root = twins.shared();
    let fake_home = root.join("home");
    let source = root.join("source");
    std::fs::create_dir_all(&fake_home).expect("fake home");
    std::fs::create_dir_all(&source).expect("fake source");
    let tools = root.join("tools");
    fake_git(&tools);
    let home = fake_home.to_string_lossy().into_owned();
    let source = source.to_string_lossy().into_owned();
    let path = tools.display().to_string();
    let (code, out, err) = shell_bind(&twins.shell_home, &home, &source, &path, true);
    let (rust_out, rust_err) = rust_bind(&home, &source, &path, true);
    assert_eq!(code, 0);
    assert_eq!((&out, &err), (&rust_out, &rust_err));
    assert_eq!(out, "code=1\n");
    assert_eq!(err, format!("dot init: {}\n", init::GIT_SHADOWED));
}

/// Shell verdict for one identity probe: `code=0/1` plus the
/// printed identity (command substitution already stripped the
/// single trailing newline, like the port's return value).
fn shell_identity(home: &Path, url: &str) -> (i32, String) {
    let snippet = format!(
        "out=$(_dot_init_repo_identity {} 2>/dev/null); code=$?; printf 'code=%s\\nout=%s\\n' \"$code\" \"$out\"\n",
        sq(url),
    );
    let (code, out, _) = shell_run(home, &snippet);
    (code, String::from_utf8_lossy(&out).into_owned())
}

/// Rust verdict in the same shape.
fn rust_identity(url: &str) -> String {
    match init::repo_identity(url) {
        Some(identity) => format!("code=0\nout={identity}\n"),
        None => "code=1\nout=\n".to_string(),
    }
}

#[test]
fn identity_resolves_file_urls() {
    let twins = Twins::build("init-ident-file-urls");
    let root = twins.shared();
    let real = root.join("real");
    std::fs::create_dir_all(&real).expect("real dir");
    std::fs::write(real.join("f"), b"x").expect("real file");
    let link = root.join("link");
    std::os::unix::fs::symlink(&real, &link).expect("dir symlink");
    let real = real.to_string_lossy().into_owned();
    let link = link.to_string_lossy().into_owned();
    let missing = root.join("missing").to_string_lossy().into_owned();
    // (input, expected identity or `None`). Like `realpath`, a
    // missing leaf still resolves against its parent — but two
    // missing components, a `..` past a missing directory, or a
    // leaf under a file all refuse.
    let root = root.to_string_lossy().into_owned();
    let rows: Vec<(String, Option<String>)> = vec![
        (format!("file://{real}"), Some(format!("file://{real}"))),
        (real.clone(), Some(format!("file://{real}"))),
        // Symlinks resolve, like `realpath`.
        (format!("file://{link}"), Some(format!("file://{real}"))),
        (link, Some(format!("file://{real}"))),
        (format!("{real}/"), Some(format!("file://{real}"))),
        ("/".to_string(), Some("file:///".to_string())),
        ("file://relative".to_string(), None),
        ("file://".to_string(), None),
        // Missing leaf, existing parent: still an identity.
        (
            format!("file://{missing}"),
            Some(format!("file://{missing}")),
        ),
        (missing.clone(), Some(format!("file://{missing}"))),
        (format!("{root}/no1/no2"), None),
        (format!("{root}/missing/../real"), None),
        (format!("{real}/f/leaf"), None),
        (String::new(), None),
        ("a\tb".to_string(), None),
        ("a\nb".to_string(), None),
        ("a\rb".to_string(), None),
    ];
    for (url, expected) in &rows {
        let (code, out) = shell_identity(&twins.shell_home, url);
        assert_eq!(code, 0, "oracle exit for {url:?}");
        assert_eq!(out, rust_identity(url), "row {url:?}");
        let want = match expected {
            Some(identity) => format!("code=0\nout={identity}\n"),
            None => "code=1\nout=\n".to_string(),
        };
        assert_eq!(out, want, "literal for {url:?}");
    }
    assert_eq!(rows.len(), 17);
}

#[test]
fn identity_normalizes_network_shapes() {
    let twins = Twins::build("init-ident-net-urls");
    // (input, expected identity or `None`). Only the host
    // lowercases, never the path; only lowercase schemes
    // special-case (uppercase falls through to the scp-like arm,
    // exactly like the shell's case-sensitive patterns).
    let rows: Vec<(&str, Option<&str>)> = vec![
        (
            "https://Example.COM/Owner/Repo.git/",
            Some("git://example.com/Owner/Repo"),
        ),
        ("http://host/a//b", Some("git://host/a//b")),
        ("https://host/a.git.git", Some("git://host/a.git")),
        ("https://host/x?y=z", Some("git://host/x?y=z")),
        ("https://host/", None),
        ("https://host", None),
        ("https:///path", None),
        ("https://user@host/x", None),
        ("https://host:8443/x", None),
        ("https://host/.git", None),
        ("ssh://git@Host/X.git", Some("git://host/X")),
        ("ssh://Host/X/", Some("git://host/X")),
        ("ssh://host/", None),
        ("ssh://host", None),
        ("ssh://a:b@host/x", None),
        ("ssh://host:22/x", None),
        ("ssh://git@host:22/x", None),
        ("git@Host:Owner/Repo.git", Some("git://host/Owner/Repo")),
        ("host:/x", Some("git://host/x")),
        ("host:path:with:colons", Some("git://host/path:with:colons")),
        ("user@host:path", Some("git://host/path")),
        ("my host:path", Some("git://my host/path")),
        ("SSH://Host/X", Some("git://ssh/Host/X")),
        ("HTTP://Example.COM/Foo", Some("git://http/Example.COM/Foo")),
        // Absolute paths resolve like `realpath`: a missing leaf
        // under `/` is still an identity, and the absolute arm wins
        // over the scp-like `host:path` reading of the colon.
        ("/abs:with:colon", Some("file:///abs:with:colon")),
        ("justastring", None),
        ("a/b/c", None),
        (":x", None),
        ("h:", None),
        ("h:/", None),
        ("user@:path", None),
        ("", None),
    ];
    for (url, expected) in &rows {
        let (code, out) = shell_identity(&twins.shell_home, url);
        assert_eq!(code, 0, "oracle exit for {url:?}");
        assert_eq!(out, rust_identity(url), "row {url:?}");
        let want = match expected {
            Some(identity) => format!("code=0\nout={identity}\n"),
            None => "code=1\nout=\n".to_string(),
        };
        assert_eq!(out, want, "literal for {url:?}");
    }
    assert_eq!(rows.len(), 32);
}

/// Shell verdict for one branch probe, normalized to 0/1: the
/// shell reports git's raw code (128 for malformed names) while the
/// port reports the boolean verdict, and every caller only branches
/// on zero versus nonzero.
fn shell_branch(home: &Path, branch: &str) -> (i32, String) {
    let snippet = format!(
        "if _dot_init_branch_valid {} 2>/dev/null; then printf 'code=0\\n'; else printf 'code=1\\n'; fi\n",
        sq(branch),
    );
    let (code, out, _) = shell_run(home, &snippet);
    (code, String::from_utf8_lossy(&out).into_owned())
}

#[test]
fn branch_validation_matrix() {
    let twins = Twins::build("init-ident-branch");
    // (input, pinned literal or `None` when the verdict belongs to
    // whatever `git check-ref-format` the engines share: both sides
    // run the same binary, so agreement is the oracle there).
    let rows: Vec<(&str, Option<&str>)> = vec![
        ("main", Some("code=0\n")),
        ("foo/bar", Some("code=0\n")),
        ("v1.0", Some("code=0\n")),
        ("", Some("code=1\n")),
        ("a..b", Some("code=1\n")),
        ("-x", Some("code=1\n")),
        ("a b", None),
        ("HEAD", None),
        ("@{1}", None),
        ("a\x7fb", None),
    ];
    for (branch, expected) in &rows {
        let (code, out) = shell_branch(&twins.shell_home, branch);
        let rust = format!("code={}\n", i32::from(!init::branch_valid(branch)));
        assert_eq!(code, 0, "oracle exit for {branch:?}");
        assert_eq!(out, rust, "row {branch:?}");
        if let Some(want) = expected {
            assert_eq!(out, *want, "literal for {branch:?}");
        }
    }
    assert_eq!(rows.len(), 10);
}

/// Shell verdict for one default-branch probe. `TMPDIR` points at
/// the engine-private scratch so probe stages never meet across
/// engines; only the branch name crosses the boundary, so random
/// stage names need no normalization.
fn shell_default(home: &Path, scratch: &Path, url: &str) -> (i32, String) {
    let snippet = format!(
        "export TMPDIR={}\nout=$(_dot_init_remote_default_branch {} 2>/dev/null); code=$?; printf 'code=%s\\nout=%s\\n' \"$code\" \"$out\"\n",
        sq(&scratch.to_string_lossy()),
        sq(url),
    );
    let (code, out, _) = shell_run(home, &snippet);
    (code, String::from_utf8_lossy(&out).into_owned())
}

/// Rust verdict in the same shape.
fn rust_default(scratch: &Path, url: &str) -> String {
    match init::remote_default_branch(url, scratch) {
        Some(branch) => format!("code=0\nout={branch}\n"),
        None => "code=1\nout=\n".to_string(),
    }
}

/// Probe stages are internal: both scratches must read empty after
/// a successful resolution.
fn assert_scratch_empty(scratch: &Path) {
    let mut leftovers: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(scratch).expect("read scratch") {
        leftovers.push(
            entry
                .expect("scratch entry")
                .file_name()
                .to_string_lossy()
                .into_owned(),
        );
    }
    assert!(leftovers.is_empty(), "stage residue: {leftovers:?}");
}

#[test]
fn default_branch_prefers_head() {
    let twins = Twins::build("init-ident-default-head");
    let origin = make_origin(twins.shared(), "origin", &["main", "other"], "main");
    let url = origin.to_string_lossy().into_owned();
    let shell_scratch = twins.shell_home.join("scratch");
    let rust_scratch = twins.rust_home.join("scratch");
    std::fs::create_dir_all(&shell_scratch).expect("shell scratch");
    std::fs::create_dir_all(&rust_scratch).expect("rust scratch");
    let (code, out) = shell_default(&twins.shell_home, &shell_scratch, &url);
    assert_eq!(code, 0);
    assert_eq!(out, rust_default(&rust_scratch, &url));
    assert_eq!(out, "code=0\nout=main\n");
    assert_scratch_empty(&shell_scratch);
    assert_scratch_empty(&rust_scratch);
    // The `file://` spelling reaches the same branch.
    let file_url = format!("file://{url}");
    let (code, out) = shell_default(&twins.shell_home, &shell_scratch, &file_url);
    assert_eq!(code, 0);
    assert_eq!(out, rust_default(&rust_scratch, &file_url));
    assert_eq!(out, "code=0\nout=main\n");
}

#[test]
fn default_branch_falls_back() {
    let twins = Twins::build("init-ident-default-fallback");
    let shell_scratch = twins.shell_home.join("scratch");
    let rust_scratch = twins.rust_home.join("scratch");
    std::fs::create_dir_all(&shell_scratch).expect("shell scratch");
    std::fs::create_dir_all(&rust_scratch).expect("rust scratch");
    // (origin branches, head, expected branch or `None`). An unborn
    // `ghost` head defeats the clone and advertisement strategies,
    // leaving the branch enumeration: `main` wins, else the lone
    // branch, else refusal.
    let rows: Vec<(&[&str], &str, Option<&str>)> = vec![
        (&["main", "other"], "ghost", Some("main")),
        (&["other", "main"], "ghost", Some("main")),
        (&["dev"], "ghost", Some("dev")),
        (&["aaa", "zzz"], "ghost", None),
        (&["main", "other"], "other", Some("other")),
    ];
    for (row, (branches, head, expected)) in rows.iter().enumerate() {
        let origin = make_origin(twins.shared(), &format!("origin-{row}"), branches, head);
        let url = origin.to_string_lossy().into_owned();
        let (code, out) = shell_default(&twins.shell_home, &shell_scratch, &url);
        assert_eq!(code, 0, "oracle exit for row {row}");
        assert_eq!(out, rust_default(&rust_scratch, &url), "row {row}");
        let want = match expected {
            Some(branch) => format!("code=0\nout={branch}\n"),
            None => "code=1\nout=\n".to_string(),
        };
        assert_eq!(out, want, "literal for row {row}");
    }
    assert_eq!(rows.len(), 5);
}

#[test]
fn default_branch_refuses_garbage() {
    let twins = Twins::build("init-ident-default-garbage");
    let shell_scratch = twins.shell_home.join("scratch");
    let rust_scratch = twins.rust_home.join("scratch");
    std::fs::create_dir_all(&shell_scratch).expect("shell scratch");
    std::fs::create_dir_all(&rust_scratch).expect("rust scratch");
    // Empty origin: nothing to advertise, nothing to enumerate.
    let empty = twins.shared().join("empty");
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg("-b")
        .arg("main")
        .arg(&empty)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git init");
    assert!(status.success(), "init {}", empty.display());
    let missing = twins.shared().join("missing");
    let urls = vec![
        empty.to_string_lossy().into_owned(),
        missing.to_string_lossy().into_owned(),
        "https://nonexistent.invalid/owner/repo".to_string(),
        String::new(),
    ];
    for url in &urls {
        let (code, out) = shell_default(&twins.shell_home, &shell_scratch, url);
        assert_eq!(code, 0, "oracle exit for {url:?}");
        assert_eq!(out, rust_default(&rust_scratch, url), "row {url:?}");
        assert_eq!(out, "code=1\nout=\n", "literal for {url:?}");
    }
    assert_eq!(urls.len(), 4);
    // A missing scratch fails like the shell's failed `mktemp`.
    let url = empty.to_string_lossy().into_owned();
    let gone_sh = twins.shell_home.join("gone");
    let gone_rs = twins.rust_home.join("gone");
    let (code, out) = shell_default(&twins.shell_home, &gone_sh, &url);
    assert_eq!(code, 0);
    assert_eq!(out, rust_default(&gone_rs, &url));
    assert_eq!(out, "code=1\nout=\n");
}
