//! Differential parity tests for the init plan-review and conflict-backup
//! family (`lib/dot/init-client.sh`, `_dot_init_confirm` through
//! `_dot_init_publish_completed`) against the live shell: the backup
//! confirmation prompt, the plan summary, the conflict move into the
//! backup, the backup restore, and the completion-record publication.
//!
//! Separate binary because each row drives real filesystem state: the
//! two engines work under disjoint home directories, so moves, sibling
//! temps, and completion markers never collide.
//!
//! Row inputs that embed device/inode identities (conflict manifests)
//! are built per side by asking the live shell to snapshot that side's
//! own files, so each engine always verifies identities it can meet.
//! The tree matcher itself (`_dot_init_path_state_matches`, owned by
//! the unmerged candidate lane) is invoked through the shell on both
//! sides, keeping these rows about the move/restore logic rather than
//! twinning another lane's verifier.
//!
//! The confirmation rows assume no controlling terminal (like CI): with
//! an empty manifest or `--yes` nothing is read, while without `--yes`
//! both engines take the non-interactive diagnostic path. Under a local
//! terminal the last confirm row prompts on `/dev/tty` on both sides —
//! answering `n` yields the expected verdict. The plan rows assume the
//! ambient `DOT_SHDEPS_UPDATE_POLICY`/`DOT_EXTENSION_API`/
//! `DOT_EXTENSIONS_DIR` overrides are unset, matching the cleared shell
//! harness environment.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::init_client_plan as plan;
use dot::temp::MoveCache;
use dot::test_support::{TempDir, bash};

/// Sources for the init plan chapter: the resource runtime, the shared
/// temp helpers (sibling temps, moves, identity), the XDG resolver,
/// and the init client itself.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/init-client.sh\"\n",
);

/// Portable aftermath dump: one line per entry under `dir`, sorted by
/// `find` plus `LC_ALL=C sort` on the shell side and the same byte
/// order on the Rust side. Regular-file bytes print inline; fixtures
/// stay ASCII so no normalization escapes are needed.
const DUMP_FN: &str = concat!(
    "dump_tree() { d=$1; find \"$d\" -mindepth 1 -print | LC_ALL=C sort | ",
    "while IFS= read -r e; do rel=${e#\"$d\"/}; ",
    "if [[ -L $e ]]; then printf 'L %s -> %s\\n' \"$rel\" \"$(readlink \"$e\")\"; ",
    "elif [[ -d $e ]]; then printf 'D %s %s\\n' \"$rel\" ",
    "\"$(stat -c '%a' \"$e\" 2>/dev/null || stat -f '%Lp' \"$e\" 2>/dev/null)\"; ",
    "elif [[ -f $e ]]; then printf 'F %s %s ' \"$rel\" ",
    "\"$(stat -c '%a' \"$e\" 2>/dev/null || stat -f '%Lp' \"$e\" 2>/dev/null)\"; ",
    "cat \"$e\"; printf '\\n'; else printf '? %s\\n' \"$rel\"; fi; done; }\n",
);

/// Run one shell snippet with the init runtime sourced and report the
/// verdict the snippet printed. Every probe ends with
/// `printf 'code=%s\n' "$code"`, so the returned code is that verdict
/// — not the process status, which only says the printer ran. A
/// snippet that never reports (a harness bug, never a pass) yields 99.
///
/// The locale stays pinned and the environment stays cleared, exactly
/// like the established slice harness; run-identity values cross as
/// explicit environment entries.
fn shell_run(home: &Path, env: &[(&str, &str)], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{DUMP_FN}{snippet}"));
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
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn bash");
    let verdict = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            line.strip_prefix("code=")
                .and_then(|code| code.parse().ok())
        })
        .unwrap_or(99);
    (verdict, output.stdout, output.stderr)
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Twin homes: disjoint directories so moves, sibling temps, and
/// completion markers never collide across engines.
/// Sort aftermath lines before comparing: sibling-temp names differ
/// across engines (shell `mktemp` versus port temps), so raw dump
/// order — correct per engine — cannot match. Sorting compares the
/// same multiset instead; stderr keeps its order-sensitive compare.
fn sorted_lines(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    lines.sort();
    lines.join("\n")
}

struct Twins {
    _dir: TempDir,
    shell_home: PathBuf,
    rust_home: PathBuf,
    shell_text: String,
    rust_text: String,
}

impl Twins {
    fn build(tag: &str) -> Self {
        let dir = TempDir::new(tag).expect("temp dir");
        let shell_home = dir.path().join("sh-home");
        let rust_home = dir.path().join("rs-home");
        std::fs::create_dir_all(&shell_home).expect("shell home");
        std::fs::create_dir_all(&rust_home).expect("rust home");
        let shell_text = shell_home.to_string_lossy().into_owned();
        let rust_text = rust_home.to_string_lossy().into_owned();
        Self {
            _dir: dir,
            shell_home,
            rust_home,
            shell_text,
            rust_text,
        }
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

/// Write `bytes` to `path`, creating parents, and force `mode`.
fn place(path: &Path, bytes: &[u8], mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("fixture parents");
    }
    std::fs::write(path, bytes).expect("write fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// Snapshot one home-relative path through the live shell: returns the
/// `kind\tdev\tino\tmode\tsize\tvalue` state for a manifest row.
fn snapshot_state(home: &Path, rel: &str) -> String {
    let target = format!("{}/{}", home.to_string_lossy(), rel);
    let snippet = format!(
        "state=$(_dot_init_snapshot_path {}); code=$?; printf '%s\\n' \"$state\"; printf 'code=%s\\n' \"$code\";",
        sq(&target),
    );
    let (code, out, _) = shell_run(home, &[], &snippet);
    assert_eq!(code, 0, "snapshot {rel}");
    let text = String::from_utf8(out).expect("snapshot dump");
    text.lines().next().expect("snapshot row").to_string()
}

/// One manifest row for `rel` under `home`.
fn manifest_row(home: &Path, rel: &str) -> String {
    format!("{rel}\t{}", snapshot_state(home, rel))
}

/// Ask the live shell whether the tree state matches: the candidate
/// lane's matcher, used as the oracle behind the Rust closures so
/// these rows exercise move/restore logic rather than a test-local
/// twin of another lane's verifier.
fn shell_matches(home: &Path, path: &Path, fields: &[&str; 6]) -> bool {
    let snippet = format!(
        "_dot_init_path_state_matches {} {} {} {} {} {} {}; code=$?; printf 'code=%s\\n' \"$code\";",
        sq(&path.to_string_lossy()),
        sq(fields[0]),
        sq(fields[1]),
        sq(fields[2]),
        sq(fields[3]),
        sq(fields[4]),
        sq(fields[5]),
    );
    shell_run(home, &[], &snippet).0 == 0
}

/// Scrub device/inode identities from one dump line: manifest rows
/// embed the snapshotting side's identities, so the `dev`/`ino`
/// fields (positions 2 and 3 of a seven-field row with a known
/// snapshot kind) collapse while mode, size, and value still compare.
/// Other lines pass through untouched.
fn scrub_manifest_identities(line: &str) -> String {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() == 7 && matches!(fields[1], "regular" | "symlink" | "directory" | "absent") {
        [
            fields[0], fields[1], "@DEV@", "@INO@", fields[4], fields[5], fields[6],
        ]
        .join("\t")
    } else {
        line.to_string()
    }
}

/// Replace side-local paths and random sibling suffixes so twin dumps
/// compare. Residue sibling temps carry random names on both engines,
/// and even their fixed parts differ (shell `mktemp` versus the port's
/// sibling temps); either whole shape collapses to `@RESIDUE@`.
/// Manifest identities scrub positionally per line.
fn normalize(text: &str, home: &str) -> String {
    let text = text.replace(home, "@HOME@");
    let text = text
        .lines()
        .map(scrub_manifest_identities)
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return text;
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        let rest = &text[index..];
        let mut consumed = None;
        for shape in [".completed.", "completed.tmp."] {
            if let Some(tail) = rest.strip_prefix(shape) {
                let run: usize = tail
                    .chars()
                    .take_while(|ch| ch.is_ascii_alphanumeric())
                    .map(|ch| ch.len_utf8())
                    .sum();
                if run >= 4 {
                    out.push_str("@RESIDUE@");
                    consumed = Some(shape.len() + run);
                    break;
                }
            }
        }
        if let Some(skip) = consumed {
            index += skip;
        } else {
            let next = rest.chars().next().expect("nonempty rest");
            out.push(next);
            index += next.len_utf8();
        }
    }
    out
}

/// Rust mirror of the shell `dump_tree` probe: same line shapes, same
/// byte order as `LC_ALL=C sort` (relative-path bytes, matching the
/// shell's full-path order under the shared directory prefix).
fn rust_dump(dir: &Path) -> String {
    let mut entries = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let read = match std::fs::read_dir(&current) {
            Ok(read) => read,
            Err(_) => continue,
        };
        for entry in read {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.is_dir() && !path.is_symlink() {
                stack.push(path.clone());
            }
            let rel = path
                .strip_prefix(dir)
                .expect("dump rel")
                .to_string_lossy()
                .into_owned();
            entries.push((rel, path));
        }
    }
    entries.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut out = String::new();
    for (rel, path) in entries {
        let meta = std::fs::symlink_metadata(&path).expect("dump stat");
        use std::os::unix::fs::MetadataExt as _;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path).expect("dump link");
            out.push_str(&format!("L {rel} -> {}\n", target.to_string_lossy()));
        } else if meta.is_dir() {
            out.push_str(&format!("D {rel} {:o}\n", meta.mode() & 0o7777));
        } else if meta.is_file() {
            let bytes = std::fs::read(&path).expect("dump read");
            out.push_str(&format!(
                "F {rel} {:o} {}\n",
                meta.mode() & 0o7777,
                String::from_utf8_lossy(&bytes)
            ));
        } else {
            out.push_str(&format!("? {rel}\n"));
        }
    }
    out
}

/// True when `/dev/tty` passes the access-bit gate but cannot be
/// opened (a container shape with no controlling terminal behind a
/// present node): the shell proceeds past its gate, so its own
/// redirection diagnostics trail the listing — script path plus line
/// numbers no port can reproduce byte for byte.
fn tty_gate_without_terminal() -> bool {
    let gate = Command::new("sh")
        .arg("-c")
        .arg("test -r /dev/tty && test -w /dev/tty")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    gate && std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_err()
}

/// Run `_dot_init_confirm` on both engines and compare the verdict
/// plus stderr. `yes_arg` renders as the shell's second argument.
fn check_confirm_row(tag: &str, manifest_bytes: Option<&[u8]>, yes_arg: &str) {
    let twins = Twins::build(&format!("confirm-{tag}"));
    for home in [&twins.shell_home, &twins.rust_home] {
        if let Some(bytes) = manifest_bytes {
            std::fs::write(home.join("conflicts.tsv"), bytes).expect("manifest");
        }
    }
    let snippet = format!(
        "manifest={}; _dot_init_confirm \"$manifest\" {}; code=$?; printf 'code=%s\\n' \"$code\";",
        sq(&format!("{}/conflicts.tsv", twins.shell_text)),
        yes_arg,
    );
    let (code, _, shell_err) = shell_run(&twins.shell_home, &[], &snippet);
    assert_ne!(code, 99, "harness verdict for confirm {tag}");

    let manifest = twins.rust_home.join("conflicts.tsv");
    let mut rust_err = Vec::new();
    let outcome = plan::confirm(&manifest, yes_arg == "true", &mut rust_err);
    let rust_code = if outcome.is_ok() { 0 } else { 1 };
    assert_eq!(rust_code, code, "confirm rc for {tag}");
    let rust_text = String::from_utf8_lossy(&rust_err).into_owned();
    let shell_text = String::from_utf8_lossy(&shell_err).into_owned();
    if tag == "listed-no-yes-noninteractive" && tty_gate_without_terminal() {
        assert!(
            normalize(&shell_text, &twins.shell_text)
                .starts_with(&normalize(&rust_text, &twins.rust_text)),
            "shell listing still matches in the exotic terminal shape"
        );
        return;
    }
    assert_eq!(
        normalize(&rust_text, &twins.rust_text),
        normalize(&shell_text, &twins.shell_text),
        "confirm stderr for {tag}"
    );
}

#[test]
fn confirm_rows_agree() {
    check_confirm_row("empty", Some(b""), "false");
    check_confirm_row("missing", None, "false");
    check_confirm_row(
        "listed-yes",
        Some(b"sub/file.txt\tregular\t1\t2\t644\t3\tabc\nweird-line\n\n"),
        "true",
    );
    check_confirm_row(
        "listed-no-yes-noninteractive",
        Some(b"sub/file.txt\tregular\t1\t2\t644\t3\tabc\n"),
        "false",
    );
}

/// Candidate fixture: `git init -b main` plus one commit. With
/// `config`, `.config/dot/config` carries those bytes.
fn seed_candidate(dir: &Path, config: Option<&[u8]>) {
    git(dir, &["init", "-q", "-b", "main"]);
    if let Some(bytes) = config {
        place(&dir.join(".config/dot/config"), bytes, 0o644);
    } else {
        place(&dir.join("seed.txt"), b"seed\n", 0o644);
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "seed"]);
}

/// Run `_dot_init_plan_summary` on both engines and compare the
/// verdict plus stderr. `compare_stderr` is false only for the
/// unreadable-tree row, whose `wc` diagnostic is platform-specific
/// (documented in [`plan::plan_summary`]).
fn check_plan_row(
    tag: &str,
    tree_bytes: Option<&[u8]>,
    config: Option<&[u8]>,
    branch: &str,
    skip_provider: bool,
    compare_stderr: bool,
) {
    let twins = Twins::build(&format!("plan-{tag}"));
    for home in [&twins.shell_home, &twins.rust_home] {
        let candidate = home.join("candidate");
        std::fs::create_dir_all(&candidate).expect("candidate dir");
        if config.is_some() || tag != "plain-dir" {
            seed_candidate(&candidate, config);
        }
        if let Some(bytes) = tree_bytes {
            std::fs::write(home.join("tree.tsv"), bytes).expect("tree");
        }
    }
    let skip_flag = if skip_provider { "1" } else { "0" };
    let snippet = format!(
        "DOT_INIT_SKIP_PROVIDER={skip_flag} _dot_init_plan_summary {} {} {} {} {}; code=$?; printf 'code=%s\\n' \"$code\";",
        sq(&format!("{}/candidate", twins.shell_text)),
        sq(branch),
        sq(&format!("{}/tree.tsv", twins.shell_text)),
        sq(&format!("{}/backup", twins.shell_text)),
        sq("example.com/dot"),
    );
    let (code, _, shell_err) = shell_run(&twins.shell_home, &[], &snippet);
    assert_ne!(code, 99, "harness verdict for plan {tag}");

    let rust_candidate = twins.rust_home.join("candidate");
    let rust_tree = twins.rust_home.join("tree.tsv");
    let rust_backup = twins.rust_home.join("backup");
    let inputs = plan::PlanInputs {
        candidate: &rust_candidate,
        branch,
        tree: &rust_tree,
        backup: &rust_backup,
        identity: "example.com/dot",
        home: &twins.rust_home,
        source_root: Path::new(env!("CARGO_MANIFEST_DIR")),
        skip_provider,
    };
    let mut rust_err = Vec::new();
    let outcome = plan::plan_summary(&inputs, &mut rust_err);
    let rust_code = if outcome.is_ok() { 0 } else { 1 };
    assert_eq!(rust_code, code, "plan rc for {tag}");
    if compare_stderr {
        assert_eq!(
            normalize(&String::from_utf8_lossy(&rust_err), &twins.rust_text),
            normalize(&String::from_utf8_lossy(&shell_err), &twins.shell_text),
            "plan stderr for {tag}"
        );
    }
}

#[test]
fn plan_summary_rows_agree() {
    check_plan_row(
        "plain-dir",
        Some(b"100644 aaa f\n100755 bbb g\n"),
        None,
        "main",
        false,
        true,
    );
    check_plan_row(
        "no-config",
        Some(b"100644 aaa f\n"),
        None,
        "main",
        false,
        true,
    );
    check_plan_row(
        "wrong-branch",
        Some(b"100644 aaa f\n"),
        None,
        "other",
        false,
        true,
    );
    check_plan_row(
        "minimal-config",
        Some(b"100644 aaa f\n100644 bbb g"),
        Some(b"version=1\n"),
        "main",
        false,
        true,
    );
    check_plan_row(
        "full-config",
        Some(b"100644 aaa f\n"),
        Some(
            b"version=1\ndependency_provider=shdeps\nshdeps_update_policy=latest\nextension_api=1\nextensions_dir=ext\n",
        ),
        "main",
        false,
        true,
    );
    check_plan_row(
        "skipped-provider",
        Some(b"100644 aaa f\n"),
        Some(b"version=1\ndependency_provider=shdeps\n"),
        "main",
        true,
        true,
    );
    check_plan_row(
        "skipped-none-provider",
        Some(b"100644 aaa f\n"),
        Some(b"version=1\n"),
        "main",
        true,
        true,
    );
    check_plan_row(
        "bad-config",
        Some(b"100644 aaa f\n"),
        Some(b"bogus\n"),
        "main",
        false,
        true,
    );
    check_plan_row("missing-tree", None, None, "main", false, false);
    check_plan_row("empty-tree", Some(b""), None, "main", false, true);
}

/// Seed conflicting home paths: a nested regular file, a nested
/// symlink, and an empty directory. Manifest rows list them in this
/// order so failure rows can park the early rows before failing.
fn seed_conflicts(home: &Path) {
    place(&home.join("sub/file.txt"), b"home bytes\n", 0o644);
    std::os::unix::fs::symlink("file.txt", home.join("sub/link")).expect("symlink fixture");
    std::fs::create_dir_all(home.join("emptydir")).expect("dir fixture");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(
        home.join("emptydir"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod fixture");
}

/// Manifest bytes for the conflict seed under `home`, snapshotted
/// per side so device/inode identities always match that side.
fn conflict_manifest(home: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    for rel in ["emptydir", "sub/file.txt", "sub/link"] {
        out.extend_from_slice(manifest_row(home, rel).as_bytes());
        out.push(b'\n');
    }
    out
}

/// Shell side of a move row: move, move again for idempotency, then
/// report both verdicts plus the stored-manifest comparison and a
/// home dump (which includes the backup subtree).
fn shell_move_twice(home: &Path, home_text: &str) -> (i32, i32, String, String, String) {
    let snippet = format!(
        "manifest={} backup={}; _dot_init_move_conflicts \"$manifest\" \"$backup\"; code=$?; printf 'code=%s\\n' \"$code\"; _dot_init_move_conflicts \"$manifest\" \"$backup\"; code2=$?; if cmp -s \"$manifest\" \"$backup/manifest\"; then stored=same; else stored=diff; fi; printf 'code2=%s stored=%s\\n' \"$code2\" \"$stored\"; dump_tree {};",
        sq(&format!("{home_text}/conflicts.tsv")),
        sq(&format!("{home_text}/backup")),
        sq(home_text),
    );
    let (code, out, err) = shell_run(home, &[], &snippet);
    assert_ne!(code, 99, "harness verdict for move");
    let text = String::from_utf8(out).expect("move dump");
    let code2_line = text
        .lines()
        .find(|line| line.starts_with("code2="))
        .unwrap_or("code2=99 stored=??");
    let code2 = code2_line
        .strip_prefix("code2=")
        .and_then(|head| head.split(' ').next())
        .and_then(|code| code.parse().ok())
        .unwrap_or(99);
    let stored = code2_line
        .split(' ')
        .find_map(|word| word.strip_prefix("stored="))
        .unwrap_or("??")
        .to_string();
    let dump = text
        .lines()
        .skip_while(|line| !line.starts_with("code2="))
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    (
        code,
        code2,
        stored,
        dump,
        String::from_utf8(err).expect("move err"),
    )
}

/// Rust side of a move row, mirroring [`shell_move_twice`].
fn rust_move_twice(home: &Path) -> (i32, i32, String, Vec<u8>) {
    let manifest = home.join("conflicts.tsv");
    let backup = home.join("backup");
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let matches =
        |path: &Path, kind: &str, dev: &str, ino: &str, mode: &str, size: &str, value: &str| {
            shell_matches(home, path, &[kind, dev, ino, mode, size, value])
        };
    let mut cache = MoveCache::default();
    let mut move_err = Vec::new();
    let first = plan::move_conflicts(
        &manifest,
        &backup,
        home,
        source_root,
        &matches,
        &mut cache,
        &mut move_err,
    );
    let second = plan::move_conflicts(
        &manifest,
        &backup,
        home,
        source_root,
        &matches,
        &mut cache,
        &mut move_err,
    );
    let stored = match (
        std::fs::read(&manifest),
        std::fs::read(backup.join("manifest")),
    ) {
        (Ok(left), Ok(right)) if left == right => "same",
        _ => "diff",
    };
    let mut dump = format!("code2={} stored={stored}\n", i32::from(second.is_err()));
    dump.push_str(&rust_dump(home));
    (
        i32::from(first.is_err()),
        i32::from(second.is_err()),
        dump,
        move_err,
    )
}

#[test]
fn move_conflicts_rows_agree() {
    let twins = Twins::build("move-fresh");
    for home in [&twins.shell_home, &twins.rust_home] {
        seed_conflicts(home);
        let manifest = conflict_manifest(home);
        std::fs::write(home.join("conflicts.tsv"), &manifest).expect("manifest");
    }
    let (shell_code, shell_code2, shell_stored, shell_dump, _) =
        shell_move_twice(&twins.shell_home, &twins.shell_text);
    let (rust_code, rust_code2, rust_dump_text, rust_move_err) = rust_move_twice(&twins.rust_home);
    assert_eq!(rust_code, shell_code, "move rc");
    assert_eq!(rust_code2, shell_code2, "move rerun rc");
    assert!(rust_move_err.is_empty(), "fresh moves stay silent");
    assert_eq!(shell_stored, "same", "shell parks the stored manifest");
    assert_eq!(
        sorted_lines(&normalize(&rust_dump_text, &twins.rust_text)),
        sorted_lines(&normalize(
            &format!("code2={shell_code2} stored={shell_stored}\n{shell_dump}"),
            &twins.shell_text
        )),
        "move aftermath"
    );
}

/// Single-shot move rows with a per-row home mutation between the
/// manifest snapshot and the move.
fn check_move_row(tag: &str, mutate: fn(&Path), want_rc: i32) {
    let twins = Twins::build(&format!("move-{tag}"));
    for home in [&twins.shell_home, &twins.rust_home] {
        seed_conflicts(home);
        let manifest = conflict_manifest(home);
        std::fs::write(home.join("conflicts.tsv"), &manifest).expect("manifest");
        mutate(home);
    }
    let snippet = format!(
        "_dot_init_move_conflicts {} {}; code=$?; printf 'code=%s\\n' \"$code\"; dump_tree {};",
        sq(&format!("{}/conflicts.tsv", twins.shell_text)),
        sq(&format!("{}/backup", twins.shell_text)),
        sq(&twins.shell_text),
    );
    let (code, out, shell_err) = shell_run(&twins.shell_home, &[], &snippet);
    assert_ne!(code, 99, "harness verdict for move {tag}");
    assert_eq!(code, want_rc, "shell move rc for {tag}");
    let shell_dump = String::from_utf8(out).expect("move dump");

    let manifest = twins.rust_home.join("conflicts.tsv");
    let backup = twins.rust_home.join("backup");
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let home = twins.rust_home.clone();
    let matches =
        |path: &Path, kind: &str, dev: &str, ino: &str, mode: &str, size: &str, value: &str| {
            shell_matches(&home, path, &[kind, dev, ino, mode, size, value])
        };
    let mut cache = MoveCache::default();
    let mut rust_err = Vec::new();
    let outcome = plan::move_conflicts(
        &manifest,
        &backup,
        &twins.rust_home,
        source_root,
        &matches,
        &mut cache,
        &mut rust_err,
    );
    let rust_code = i32::from(outcome.is_err());
    assert_eq!(rust_code, want_rc, "rust move rc for {tag}");
    let rust_dump_text = rust_dump(&twins.rust_home);
    let shell_aftermath = shell_dump
        .lines()
        .skip_while(|line| !line.starts_with("code="))
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        sorted_lines(&normalize(&rust_dump_text, &twins.rust_text)),
        sorted_lines(&normalize(&shell_aftermath, &twins.shell_text)),
        "move aftermath for {tag}"
    );
    assert_eq!(
        normalize(&String::from_utf8_lossy(&rust_err), &twins.rust_text),
        normalize(&String::from_utf8_lossy(&shell_err), &twins.shell_text),
        "move stderr for {tag}"
    );
}

#[test]
fn move_conflicts_failure_rows_agree() {
    check_move_row(
        "missing-manifest",
        |home| {
            std::fs::remove_file(home.join("conflicts.tsv")).expect("drop manifest");
        },
        1,
    );
    check_move_row(
        "home-changed",
        |home| {
            place(&home.join("sub/file.txt"), b"changed\n", 0o644);
        },
        1,
    );
    check_move_row(
        "dest-occupied",
        |home| {
            place(&home.join("backup/sub/file.txt"), b"squatter\n", 0o644);
        },
        1,
    );
    check_move_row(
        "stored-differs",
        |home| {
            std::fs::create_dir_all(home.join("backup")).expect("backup dir");
            std::fs::write(home.join("backup/manifest"), b"junk\n").expect("junk manifest");
        },
        1,
    );
}

/// Move on both engines, then restore on both engines, comparing the
/// restore verdict plus the final home and backup dumps. `disturb`
/// runs between the move and the restore (per side) to shape failure
/// rows; the roundtrip passes a no-op.
fn check_restore_row(tag: &str, disturb: fn(&Path), want_rc: i32) {
    let twins = Twins::build(&format!("restore-{tag}"));
    for home in [&twins.shell_home, &twins.rust_home] {
        seed_conflicts(home);
        let manifest = conflict_manifest(home);
        std::fs::write(home.join("conflicts.tsv"), &manifest).expect("manifest");
    }
    let snippet = format!(
        "manifest={} backup={}; _dot_init_move_conflicts \"$manifest\" \"$backup\" || {{ code=$?; printf 'code=%s\\n' \"$code\"; exit 0; }}; dump_tree {} >/dev/null; ",
        sq(&format!("{}/conflicts.tsv", twins.shell_text)),
        sq(&format!("{}/backup", twins.shell_text)),
        sq(&twins.shell_text),
    );
    let (move_code, _, _) = shell_run(
        &twins.shell_home,
        &[],
        &format!("{snippet}printf 'code=0\\n';"),
    );
    assert_eq!(move_code, 0, "shell setup move for restore {tag}");
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    {
        let manifest = twins.rust_home.join("conflicts.tsv");
        let backup = twins.rust_home.join("backup");
        let home = twins.rust_home.clone();
        let matches =
            |path: &Path, kind: &str, dev: &str, ino: &str, mode: &str, size: &str, value: &str| {
                shell_matches(&home, path, &[kind, dev, ino, mode, size, value])
            };
        let mut cache = MoveCache::default();
        let mut setup_err = Vec::new();
        plan::move_conflicts(
            &manifest,
            &backup,
            &twins.rust_home,
            source_root,
            &matches,
            &mut cache,
            &mut setup_err,
        )
        .expect("rust setup move");
        assert!(setup_err.is_empty(), "setup moves stay silent");
    }
    for home in [&twins.shell_home, &twins.rust_home] {
        disturb(home);
    }
    let restore_snippet = format!(
        "_dot_init_restore_backups {}; code=$?; printf 'code=%s\\n' \"$code\"; dump_tree {}; dump_tree {};",
        sq(&format!("{}/backup", twins.shell_text)),
        sq(&twins.shell_text),
        sq(&format!("{}/backup", twins.shell_text)),
    );
    let (code, out, shell_err) = shell_run(&twins.shell_home, &[], &restore_snippet);
    assert_ne!(code, 99, "harness verdict for restore {tag}");
    assert_eq!(code, want_rc, "shell restore rc for {tag}");
    let shell_dump = String::from_utf8(out).expect("restore dump");

    let backup = twins.rust_home.join("backup");
    let home = twins.rust_home.clone();
    let matches =
        |path: &Path, kind: &str, dev: &str, ino: &str, mode: &str, size: &str, value: &str| {
            shell_matches(&home, path, &[kind, dev, ino, mode, size, value])
        };
    let mut cache = MoveCache::default();
    let mut rust_err = Vec::new();
    let outcome = plan::restore_backups(
        &backup,
        &twins.rust_home,
        &matches,
        &mut cache,
        &mut rust_err,
    );
    let rust_code = i32::from(outcome.is_err());
    assert_eq!(rust_code, want_rc, "rust restore rc for {tag}");
    let mut rust_dump_text = rust_dump(&twins.rust_home);
    rust_dump_text.push_str(&rust_dump(&backup));
    let shell_aftermath = shell_dump
        .lines()
        .skip_while(|line| !line.starts_with("code="))
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        sorted_lines(&normalize(&rust_dump_text, &twins.rust_text)),
        sorted_lines(&normalize(&shell_aftermath, &twins.shell_text)),
        "restore aftermath for {tag}"
    );
    assert_eq!(
        normalize(&String::from_utf8_lossy(&rust_err), &twins.rust_text),
        normalize(&String::from_utf8_lossy(&shell_err), &twins.shell_text),
        "restore stderr for {tag}"
    );
}

#[test]
fn restore_backups_rows_agree() {
    check_restore_row("roundtrip", |_| {}, 0);
    check_restore_row(
        "parked-changed",
        |home| {
            place(&home.join("backup/sub/file.txt"), b"tampered\n", 0o644);
        },
        1,
    );
    check_restore_row(
        "home-occupied",
        |home| {
            place(&home.join("sub/file.txt"), b"squatter\n", 0o644);
        },
        1,
    );
}

#[test]
fn restore_backups_edge_rows_agree() {
    let twins = Twins::build("restore-edge");
    for home in [&twins.shell_home, &twins.rust_home] {
        std::fs::create_dir_all(home.join("backup")).expect("backup dir");
        std::fs::write(
            home.join("backup/manifest"),
            b"../evil\tdirectory\t1\t2\t700\t4096\t-\n",
        )
        .expect("evil manifest");
    }
    let snippet = format!(
        "_dot_init_restore_backups {}; code=$?; printf 'code=%s\\n' \"$code\";",
        sq(&format!("{}/backup", twins.shell_text)),
    );
    let (code, _, _) = shell_run(&twins.shell_home, &[], &snippet);
    assert_eq!(code, 1, "shell rejects the unsafe path");
    let backup = twins.rust_home.join("backup");
    let home = twins.rust_home.clone();
    let matches =
        |path: &Path, kind: &str, dev: &str, ino: &str, mode: &str, size: &str, value: &str| {
            shell_matches(&home, path, &[kind, dev, ino, mode, size, value])
        };
    let mut cache = MoveCache::default();
    let mut edge_err = Vec::new();
    assert!(
        plan::restore_backups(
            &backup,
            &twins.rust_home,
            &matches,
            &mut cache,
            &mut edge_err
        )
        .is_err(),
        "rust rejects the unsafe path"
    );
    assert!(edge_err.is_empty(), "unsafe-path rejections stay silent");

    let absent = Twins::build("restore-absent");
    let absent_snippet = format!(
        "_dot_init_restore_backups {}; code=$?; printf 'code=%s\\n' \"$code\"; [[ -e {} ]] && dump_tree {};",
        sq(&format!("{}/backup", absent.shell_text)),
        sq(&format!("{}/backup", absent.shell_text)),
        sq(&format!("{}/backup", absent.shell_text)),
    );
    let (absent_code, absent_out, _) = shell_run(&absent.shell_home, &[], &absent_snippet);
    assert_eq!(absent_code, 0, "shell restores nothing without a backup");
    let absent_dump = String::from_utf8(absent_out).expect("absent dump");
    let absent_aftermath = absent_dump
        .lines()
        .skip_while(|line| !line.starts_with("code="))
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    let missing = absent.rust_home.join("backup");
    let missing_home = absent.rust_home.clone();
    let missing_matches =
        |path: &Path, kind: &str, dev: &str, ino: &str, mode: &str, size: &str, value: &str| {
            shell_matches(&missing_home, path, &[kind, dev, ino, mode, size, value])
        };
    let mut missing_cache = MoveCache::default();
    let mut missing_err = Vec::new();
    assert!(
        plan::restore_backups(
            &missing,
            &absent.rust_home,
            &missing_matches,
            &mut missing_cache,
            &mut missing_err
        )
        .is_ok(),
        "rust restores nothing without a backup"
    );
    assert!(missing_err.is_empty(), "absent restores stay silent");
    assert_eq!(
        sorted_lines(&normalize(&rust_dump(&absent.rust_home), &absent.rust_text)),
        sorted_lines(&normalize(&absent_aftermath, &absent.shell_text)),
        "absent restore aftermath"
    );
}

/// Cross-engine lifecycle: the Rust move parks shell-side conflicts
/// for the shell restore, and vice versa on the twin side.
#[test]
fn move_restore_interop_agrees() {
    let twins = Twins::build("interop");
    for home in [&twins.shell_home, &twins.rust_home] {
        seed_conflicts(home);
        let manifest = conflict_manifest(home);
        std::fs::write(home.join("conflicts.tsv"), &manifest).expect("manifest");
    }
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let shell_side = twins.shell_home.clone();
    let shell_matches_here =
        |path: &Path, kind: &str, dev: &str, ino: &str, mode: &str, size: &str, value: &str| {
            shell_matches(&shell_side, path, &[kind, dev, ino, mode, size, value])
        };
    let mut cache = MoveCache::default();
    let mut interop_err = Vec::new();
    plan::move_conflicts(
        &twins.shell_home.join("conflicts.tsv"),
        &twins.shell_home.join("backup"),
        &twins.shell_home,
        source_root,
        &shell_matches_here,
        &mut cache,
        &mut interop_err,
    )
    .expect("rust moves shell-side conflicts");
    assert!(interop_err.is_empty(), "interop moves stay silent");
    let restore_snippet = format!(
        "_dot_init_restore_backups {}; code=$?; printf 'code=%s\\n' \"$code\"; dump_tree {};",
        sq(&format!("{}/backup", twins.shell_text)),
        sq(&twins.shell_text),
    );
    let (shell_code, shell_out, _) = shell_run(&twins.shell_home, &[], &restore_snippet);
    assert_eq!(shell_code, 0, "shell restores rust-parked conflicts");

    let move_snippet = format!(
        "_dot_init_move_conflicts {} {}; code=$?; printf 'code=%s\\n' \"$code\";",
        sq(&format!("{}/conflicts.tsv", twins.rust_text)),
        sq(&format!("{}/backup", twins.rust_text)),
    );
    let (move_code, _, _) = shell_run(&twins.rust_home, &[], &move_snippet);
    assert_eq!(move_code, 0, "shell moves rust-side conflicts");
    let rust_side = twins.rust_home.clone();
    let rust_matches_here =
        |path: &Path, kind: &str, dev: &str, ino: &str, mode: &str, size: &str, value: &str| {
            shell_matches(&rust_side, path, &[kind, dev, ino, mode, size, value])
        };
    let mut rust_cache = MoveCache::default();
    let mut restore_err = Vec::new();
    plan::restore_backups(
        &twins.rust_home.join("backup"),
        &twins.rust_home,
        &rust_matches_here,
        &mut rust_cache,
        &mut restore_err,
    )
    .expect("rust restores shell-parked conflicts");
    assert!(restore_err.is_empty(), "interop restores stay silent");

    let shell_aftermath = String::from_utf8(shell_out).expect("interop dump");
    let shell_dump = shell_aftermath
        .lines()
        .skip_while(|line| !line.starts_with("code="))
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        sorted_lines(&normalize(&rust_dump(&twins.rust_home), &twins.rust_text)),
        sorted_lines(&normalize(&shell_dump, &twins.shell_text)),
        "interop aftermath"
    );
}

/// Pre-state of the completion marker for publish rows.
#[derive(Clone, Copy)]
enum CompletedSeed {
    /// No marker yet: the exclusive publish path.
    Absent,
    /// A stale regular marker we own: the replace path.
    Stale,
    /// A symlink marker: both engines refuse and leave residue.
    Symlink,
    /// A directory marker: both engines refuse and leave residue.
    Dir,
}

/// Run `_dot_init_publish_completed` on both engines and compare the
/// verdict plus the state-root dump (marker bytes, modes, and any
/// sibling residue, normalized). With `use_xdg` the marker lives
/// under `$XDG_STATE_HOME`; otherwise the home fallback applies.
fn check_publish_row(
    tag: &str,
    use_xdg: bool,
    record: Option<&[u8]>,
    seed: CompletedSeed,
    want_rc: i32,
) {
    let twins = Twins::build(&format!("publish-{tag}"));
    for home in [&twins.shell_home, &twins.rust_home] {
        let record_path = home.join("record");
        if let Some(bytes) = record {
            place(&record_path, bytes, 0o644);
        }
        let root = if use_xdg {
            home.join("xdg-state/dot/init")
        } else {
            home.join(".local/state/dot/init")
        };
        let completed = root.join("completed");
        match seed {
            CompletedSeed::Absent => {}
            CompletedSeed::Stale => {
                place(&completed, b"stale marker\n", 0o600);
            }
            CompletedSeed::Symlink => {
                place(&home.join("target"), b"target\n", 0o644);
                std::fs::create_dir_all(&root).expect("marker parent");
                std::os::unix::fs::symlink(home.join("target"), &completed)
                    .expect("marker symlink");
            }
            CompletedSeed::Dir => {
                std::fs::create_dir_all(&completed).expect("marker dir");
            }
        }
    }
    let shell_root = if use_xdg {
        format!("{}/xdg-state", twins.shell_text)
    } else {
        format!("{}/.local/state", twins.shell_text)
    };
    let snippet = format!(
        "record={}; _dot_init_publish_completed \"$record\"; code=$?; printf 'code=%s\\n' \"$code\"; [[ -e {} ]] && dump_tree {};",
        sq(&format!("{}/record", twins.shell_text)),
        sq(&shell_root),
        sq(&shell_root),
    );
    let xdg_text = twins
        .shell_home
        .join("xdg-state")
        .to_string_lossy()
        .into_owned();
    let shell_env: Vec<(&str, &str)> = if use_xdg {
        vec![("XDG_STATE_HOME", xdg_text.as_str())]
    } else {
        Vec::new()
    };
    let (code, out, shell_err) = shell_run(&twins.shell_home, &shell_env, &snippet);
    assert_ne!(code, 99, "harness verdict for publish {tag}");
    assert_eq!(code, want_rc, "shell publish rc for {tag}");
    let shell_dump = String::from_utf8(out).expect("publish dump");

    let rust_xdg = twins.rust_home.join("xdg-state");
    let xdg_value = if use_xdg {
        rust_xdg.to_string_lossy().into_owned()
    } else {
        String::new()
    };
    let mut cache = MoveCache::default();
    let mut rust_err = Vec::new();
    let outcome = plan::publish_completed(
        &twins.rust_home.join("record"),
        &twins.rust_text,
        &xdg_value,
        &mut cache,
        &mut rust_err,
    );
    let rust_code = i32::from(outcome.is_err());
    assert_eq!(rust_code, want_rc, "rust publish rc for {tag}");
    let rust_root = if use_xdg {
        twins.rust_home.join("xdg-state")
    } else {
        twins.rust_home.join(".local/state")
    };
    let rust_dump_text = rust_dump(&rust_root);
    let shell_aftermath = shell_dump
        .lines()
        .skip_while(|line| !line.starts_with("code="))
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        sorted_lines(&normalize(&rust_dump_text, &twins.rust_text)),
        sorted_lines(&normalize(&shell_aftermath, &twins.shell_text)),
        "publish aftermath for {tag}"
    );
    assert_eq!(
        normalize(&String::from_utf8_lossy(&rust_err), &twins.rust_text),
        normalize(&String::from_utf8_lossy(&shell_err), &twins.shell_text),
        "publish stderr for {tag}"
    );
}

#[test]
fn publish_completed_rows_agree() {
    const RECORD: &[u8] = b"cgraf78 dot initialization transaction v1\nphase=complete\n";
    check_publish_row("fresh", false, Some(RECORD), CompletedSeed::Absent, 0);
    check_publish_row("replace", false, Some(RECORD), CompletedSeed::Stale, 0);
    check_publish_row("xdg-state", true, Some(RECORD), CompletedSeed::Absent, 0);
    check_publish_row(
        "symlink-marker",
        false,
        Some(RECORD),
        CompletedSeed::Symlink,
        1,
    );
    check_publish_row("missing-record", false, None, CompletedSeed::Absent, 1);
    check_publish_row("dir-marker", false, Some(RECORD), CompletedSeed::Dir, 1);
}
