//! Differential parity tests for the publish leaf layer of
//! `lib/dot/repos/overlays.sh`: link-target recording and matching,
//! active/authority ownership checks, skip-worktree and cleanliness
//! gates, the pending manifest path, and private writers.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_overlays;
use dot::test_support::TempDir;

/// Run one shell snippet with the quarantine runtime sourced (the
/// publish leaves share its reserved/xdg dependencies).
fn shell_run(home: &Path, argv: &[&std::ffi::OsStr], snippet: &str) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tmpdir = std::env::var_os("TMPDIR")
        .filter(|dir| !dir.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp"));
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!(
            ". \"$1/lib/dot/repos/overlays.sh\"\n. \"$1/lib/dot/reserved.sh\"\n. \"$1/lib/dot/public/xdg.sh\"\n{snippet}"
        ));
    cmd.arg("dot-test-sh").arg(repo);
    for arg in argv {
        cmd.arg(arg);
    }
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

/// `name|path|url|conf|optional|sync` overlay record.
fn ov(name: &str, path: &str, sync: &str) -> String {
    format!("{name}|{path}|https://example.invalid/x|||{sync}")
}

/// Run `git -C dir args...` with inline identity, silenced.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?}");
}

#[test]
fn record_link_target_agrees() {
    let dir = TempDir::new("ovpub-record").expect("fixture dir");
    let home = dir.path();
    // (rel, name, path, sync argv): empty sync omits `$5` so the
    // shell applies its `git` default.
    for (rel, name, path, sync) in [
        ("app.conf", "web", "/ov", "git"),
        ("app.conf", "web", "/ov", ""),
        ("app.conf", "web", "/ov/path", "none"),
        ("app.conf", "web", "/ov", "bogus"),
        ("app.conf", "", "/ov", "git"),
    ] {
        let snippet = if sync.is_empty() {
            format!(
                "_overlay_record_link_target {} {} {}; code=$?; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"\n",
                sq(rel),
                sq(name),
                sq(path),
            )
        } else {
            format!(
                "_overlay_record_link_target {} {} {} {}; code=$?; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"\n",
                sq(rel),
                sq(name),
                sq(path),
                sq(sync),
            )
        };
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {rel:?}/{sync:?}");
        assert!(serr.is_empty(), "record stderr: {serr:?}");
        let shell = String::from_utf8(out).expect("record dump");
        let rust = match repos_overlays::record_link_target(
            rel,
            name,
            path,
            if sync.is_empty() { None } else { Some(sync) },
        ) {
            Some(target) => format!("rc=0\nreply={target}\n"),
            None => "rc=1\nreply=\n".to_string(),
        };
        assert_eq!(rust, shell, "record link target for {rel:?}/{sync:?}");
    }
}

#[test]
fn active_provides_agrees() {
    let dir = TempDir::new("ovpub-provides").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let ova = format!("{home_text}/ova");
    let ovb = format!("{home_text}/ovb");
    stage(home, "ova/home/app.conf", b"a\n");
    std::fs::create_dir_all(home.join("ovb/home")).expect("empty overlay home");
    // The last entry proves `sync` is ignored: a `none` overlay still
    // provides the paths under its home.
    let overlays = vec![
        ov("a", &ova, "git"),
        ov("b", &ovb, "git"),
        ov("c", &ova, "none"),
    ];
    for (rel, want) in [("app.conf", 0), ("missing.conf", 1), ("../escape", 1)] {
        let escaped: Vec<String> = overlays.iter().map(|entry| sq(entry)).collect();
        let snippet = format!(
            "OVERLAYS=({}); if _overlay_active_provides {}; then code=0; else code=1; fi; printf 'rc=%s\\n' \"$code\"\n",
            escaped.join(" "),
            sq(rel),
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {rel:?}");
        assert!(serr.is_empty(), "provides stderr: {serr:?}");
        let shell = String::from_utf8(out).expect("provides dump");
        let rust_code = if repos_overlays::active_provides(&overlays, rel) {
            0
        } else {
            1
        };
        assert_eq!(rust_code, want, "rust expectation for {rel:?}");
        assert_eq!(
            format!("rc={rust_code}\n"),
            shell,
            "active provides for {rel:?}"
        );
    }
}

#[test]
fn active_link_matches_agrees() {
    let dir = TempDir::new("ovpub-activematch").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let ova = format!("{home_text}/ova");
    stage(home, "ova/home/app.conf", b"a\n");
    std::os::unix::fs::symlink(".dotfiles-a/home/app.conf", home.join("app.conf"))
        .expect("managed link");
    std::os::unix::fs::symlink("elsewhere", home.join("rogue.conf")).expect("rogue link");
    stage(home, "ova/home/rogue.conf", b"r\n");
    let overlays = vec![ov("a", &ova, "git"), ov("n", &ova, "none")];
    for (rel, want) in [("app.conf", 0), ("rogue.conf", 1), ("missing.conf", 1)] {
        let escaped: Vec<String> = overlays.iter().map(|entry| sq(entry)).collect();
        let snippet = format!(
            "OVERLAYS=({}); if _overlay_active_link_matches {}; then code=0; else code=1; fi; printf 'rc=%s\\n' \"$code\"\n",
            escaped.join(" "),
            sq(rel),
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {rel:?}");
        assert!(serr.is_empty(), "active match stderr: {serr:?}");
        let shell = String::from_utf8(out).expect("active match dump");
        let rust_code = if repos_overlays::active_link_matches(&home_text, &overlays, rel) {
            0
        } else {
            1
        };
        assert_eq!(rust_code, want, "rust expectation for {rel:?}");
        assert_eq!(
            format!("rc={rust_code}\n"),
            shell,
            "active link matches for {rel:?}"
        );
    }
}

#[test]
fn authority_link_matches_agrees() {
    let dir = TempDir::new("ovpub-authoritymatch").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    stage(home, "target.txt", b"t\n");
    std::os::unix::fs::symlink("target.txt", home.join("owned")).expect("link");
    std::os::unix::fs::symlink("elsewhere", home.join("rogue")).expect("rogue");
    let targets = vec![("owned".to_string(), "target.txt".to_string())];
    for (rel, want) in [("owned", 0), ("rogue", 1), ("missing", 1)] {
        // The assoc key joins rel and target with a literal tab, like
        // the shell's `$rel$'\t'$target` subscription.
        let key = sq("owned\ttarget.txt");
        let snippet = format!(
            "declare -A _overlay_authority_targets=([{key}]='1'); if _overlay_authority_link_matches {rel}; then code=0; else code=1; fi; printf 'rc=%s\\n' \"$code\"\n",
            rel = sq(rel),
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {rel:?}");
        assert!(serr.is_empty(), "authority match stderr: {serr:?}");
        let shell = String::from_utf8(out).expect("authority match dump");
        let rust_code = if repos_overlays::authority_link_matches(&home_text, &targets, rel) {
            0
        } else {
            1
        };
        assert_eq!(rust_code, want, "rust expectation for {rel:?}");
        assert_eq!(
            format!("rc={rust_code}\n"),
            shell,
            "authority link matches for {rel:?}"
        );
    }
}

#[test]
fn link_matches_agrees() {
    let dir = TempDir::new("ovpub-match").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    stage(home, "target.txt", b"t\n");
    std::os::unix::fs::symlink("target.txt", home.join("plain")).expect("link");
    std::os::unix::fs::symlink("target.txt\n", home.join("nl")).expect("nl link");
    // (rel, name, explicit target): `None` takes the derived form.
    for (rel, name, explicit) in [
        ("plain", "web", Some("target.txt")),
        ("plain", "web", Some("other.txt")),
        ("plain", "web", None),
        ("nl", "web", Some("target.txt")),
        ("missing", "web", Some("target.txt")),
        ("plain", "", Some("target.txt")),
    ] {
        let snippet = match explicit {
            Some(target) => format!(
                "if _overlay_link_matches {} {} {}; then code=0; else code=1; fi; printf 'rc=%s\\n' \"$code\"\n",
                sq(rel),
                sq(name),
                sq(target),
            ),
            None => format!(
                "if _overlay_link_matches {} {}; then code=0; else code=1; fi; printf 'rc=%s\\n' \"$code\"\n",
                sq(rel),
                sq(name),
            ),
        };
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {rel:?}");
        assert!(serr.is_empty(), "match stderr: {serr:?}");
        let shell = String::from_utf8(out).expect("match dump");
        let rust_code = if repos_overlays::link_matches(&home_text, rel, name, explicit) {
            0
        } else {
            1
        };
        assert_eq!(
            format!("rc={rust_code}\n"),
            shell,
            "link matches for {rel:?}"
        );
    }
}

#[test]
fn pending_manifest_path_agrees() {
    let dir = TempDir::new("ovpub-pending").expect("fixture dir");
    let home = dir.path();
    let manifest = home.join("manifest.tsv");
    let manifest_text = manifest.to_string_lossy().into_owned();
    let (code, out, serr) = shell_run(
        home,
        &[manifest.as_os_str()],
        "DOT_OVERLAY_MANIFEST=\"$2\"; _overlay_pending_manifest_path; printf 'rc=%s\\nreply=%s\\n' \"$?\" \"$REPLY\"\n",
    );
    assert_eq!(code, 0, "harness exit");
    assert!(serr.is_empty(), "pending stderr: {serr:?}");
    let shell = String::from_utf8(out).expect("pending dump");
    assert_eq!(
        format!("rc=0\nreply={}.pending\n", manifest_text),
        shell,
        "pending manifest path"
    );
    assert_eq!(
        repos_overlays::pending_manifest_path(&manifest_text),
        format!("{manifest_text}.pending"),
        "rust pending manifest path"
    );
}

#[test]
fn path_is_authority_agrees() {
    let dir = TempDir::new("ovpub-pathauthority").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let manifest = format!("{home_text}/manifest.tsv");
    let legacy = format!("{home_text}/legacy.tsv");
    for (rel, roots) in [
        ("app.conf", "live"),
        (".config/dot/profiles.d/x", "live"),
        ("manifest.tsv", "live"),
        (".dotfiles-evil/x", "live"),
        ("sub/x", "snapshot"),
        ("elsewhere/y", "snapshot"),
    ] {
        let roots_value;
        let roots_preamble;
        let rust_roots: Option<String>;
        if roots == "snapshot" {
            roots_value = format!("{home_text}/sub\n\n{home_text}/sub\n");
            roots_preamble = format!("_overlay_reserved_roots={}; ", sq(&roots_value));
            rust_roots = Some(roots_value.clone());
        } else {
            roots_preamble = String::new();
            rust_roots = None;
        }
        let snippet = format!(
            "export DOT_OVERLAY_MANIFEST={} DOT_OVERLAY_LEGACY_MANIFEST={} XDG_STATE_HOME={} SHDEPS_INSTALL_DIR={} SHDEPS_STATE_DIR={}; _overlay_path_authority_cache_enabled=1; declare -A _overlay_path_authority_cache=(); {roots_preamble}if _overlay_path_is_authority {}; then code=0; else code=1; fi; printf 'rc=%s\\n' \"$code\"\n",
            sq(&manifest),
            sq(&legacy),
            sq(&format!("{home_text}/xdg-state")),
            sq(&format!("{home_text}/install")),
            sq(&format!("{home_text}/shdeps")),
            sq(rel),
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {rel:?}/{roots}");
        assert!(serr.is_empty(), "authority stderr: {serr:?}");
        let shell = String::from_utf8(out).expect("authority dump");
        let inputs = repos_overlays::DestinationInputs {
            pwd: home_text.clone(),
            home: home_text.clone(),
            xdg_state_home: Some(format!("{home_text}/xdg-state")),
            install_dir: Some(format!("{home_text}/install")),
            state_dir: Some(format!("{home_text}/shdeps")),
            overlay_paths: vec![],
            init_backup: None,
        };
        let mut cache = repos_overlays::AuthorityCache::enabled();
        let rust_code = if repos_overlays::path_is_authority(
            &home_text,
            rel,
            &manifest,
            &legacy,
            &inputs,
            rust_roots.as_deref(),
            &mut cache,
        ) {
            0
        } else {
            1
        };
        assert_eq!(
            format!("rc={rust_code}\n"),
            shell,
            "path is authority for {rel:?}/{roots}"
        );
    }
}

#[test]
fn path_is_authority_cache_pins() {
    let dir = TempDir::new("ovpub-cache").expect("fixture dir");
    let home = dir.path();
    let home_text = home.to_string_lossy().into_owned();
    let manifest = format!("{home_text}/manifest.tsv");
    let legacy = format!("{home_text}/legacy.tsv");
    let inputs = repos_overlays::DestinationInputs {
        pwd: home_text.clone(),
        home: home_text.clone(),
        xdg_state_home: None,
        install_dir: None,
        state_dir: None,
        overlay_paths: vec![],
        init_backup: None,
    };
    let hit = format!("{home_text}/sub");
    let snippet = format!(
        "export DOT_OVERLAY_MANIFEST={} DOT_OVERLAY_LEGACY_MANIFEST={}; _overlay_path_authority_cache_enabled=1; declare -A _overlay_path_authority_cache=(); _overlay_reserved_roots={}; if _overlay_path_is_authority 'sub/x'; then first=0; else first=1; fi; _overlay_reserved_roots=''; if _overlay_path_is_authority 'sub/x'; then second=0; else second=1; fi; printf 'first=%s\\nsecond=%s\\n' \"$first\" \"$second\"\n",
        sq(&manifest),
        sq(&legacy),
        sq(&hit),
    );
    let (code, out, serr) = shell_run(home, &[], &snippet);
    assert_eq!(code, 0, "harness exit");
    assert!(serr.is_empty(), "cache stderr: {serr:?}");
    let shell = String::from_utf8(out).expect("cache dump");
    let mut cache = repos_overlays::AuthorityCache::enabled();
    let first = repos_overlays::path_is_authority(
        &home_text,
        "sub/x",
        &manifest,
        &legacy,
        &inputs,
        Some(&hit),
        &mut cache,
    );
    let second = repos_overlays::path_is_authority(
        &home_text,
        "sub/x",
        &manifest,
        &legacy,
        &inputs,
        Some(""),
        &mut cache,
    );
    let rust = format!(
        "first={}\nsecond={}\n",
        if first { 0 } else { 1 },
        if second { 0 } else { 1 }
    );
    assert_eq!(rust, shell, "cached authority verdicts");
    assert!(first && second, "enabled cache pins the first verdict");
    let mut open = repos_overlays::AuthorityCache::disabled();
    assert!(
        repos_overlays::path_is_authority(
            &home_text,
            "sub/x",
            &manifest,
            &legacy,
            &inputs,
            Some(&hit),
            &mut open
        ),
        "disabled cache answers hit"
    );
    assert!(
        !repos_overlays::path_is_authority(
            &home_text,
            "sub/x",
            &manifest,
            &legacy,
            &inputs,
            Some(""),
            &mut open
        ),
        "disabled cache re-evaluates the miss"
    );
}

#[test]
fn skip_worktree_and_tracked_clean_agree() {
    let dir = TempDir::new("ovpub-git").expect("fixture dir");
    let home = dir.path();
    git(home, &["init", "-q"]);
    stage(home, "clean.txt", b"clean\n");
    stage(home, "skip.txt", b"skip\n");
    stage(home, "dirty.txt", b"dirty\n");
    stage(home, "untracked.txt", b"new\n");
    git(home, &["add", "-A"]);
    git(home, &["commit", "-qm", "seed"]);
    git(home, &["update-index", "--skip-worktree", "skip.txt"]);
    stage(home, "dirty.txt", b"changed\n");
    let base = dot::repos_base::Base {
        topology: dot::repos_base::Topology::Ordinary,
        client_git_dir: String::new(),
        home: home.to_string_lossy().into_owned(),
    };
    for (rel, want_skip, want_clean) in [
        ("clean.txt", false, true),
        ("skip.txt", true, false),
        ("dirty.txt", false, false),
        ("untracked.txt", false, true),
    ] {
        let snippet = format!(
            "_base_git() {{ command git -C \"$HOME\" \"$@\"; }}; if _overlay_skip_worktree {}; then skip=0; else skip=1; fi; if _overlay_tracked_path_clean {}; then clean=0; else clean=1; fi; printf 'skip=%s\\nclean=%s\\n' \"$skip\" \"$clean\"\n",
            sq(rel),
            sq(rel),
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {rel:?}");
        assert!(serr.is_empty(), "git gate stderr: {serr:?}");
        let shell = String::from_utf8(out).expect("git gate dump");
        let rust_skip = repos_overlays::skip_worktree(&base, rel);
        let rust_clean = repos_overlays::tracked_path_clean(&base, rel);
        assert_eq!(rust_skip, want_skip, "rust skip for {rel:?}");
        assert_eq!(rust_clean, want_clean, "rust clean for {rel:?}");
        assert_eq!(
            format!(
                "skip={}\nclean={}\n",
                if rust_skip { 0 } else { 1 },
                if rust_clean { 0 } else { 1 }
            ),
            shell,
            "skip/clean for {rel:?}"
        );
    }
    let missing = dot::repos_base::Base {
        topology: dot::repos_base::Topology::Missing,
        client_git_dir: String::new(),
        home: home.to_string_lossy().into_owned(),
    };
    let snippet = "_base_git() { return 128; }; if _overlay_skip_worktree 'clean.txt'; then skip=0; else skip=1; fi; if _overlay_tracked_path_clean 'clean.txt'; then clean=0; else clean=1; fi; printf 'skip=%s\\nclean=%s\\n' \"$skip\" \"$clean\"\n";
    let (code, out, serr) = shell_run(home, &[], snippet);
    assert_eq!(code, 0, "harness exit for missing topology");
    assert!(serr.is_empty(), "missing stderr: {serr:?}");
    assert!(!repos_overlays::skip_worktree(&missing, "clean.txt"));
    assert!(!repos_overlays::tracked_path_clean(&missing, "clean.txt"));
    assert_eq!(
        String::from_utf8(out).expect("missing dump"),
        "skip=1\nclean=1\n",
        "missing topology gates"
    );
}

#[test]
fn write_private_line_agrees() {
    let dir = TempDir::new("ovpub-privline").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    for side in ["shell", "rust"] {
        let root = home.join(side);
        std::fs::create_dir_all(&root).expect("side dir");
        let destination = root.join("record.tsv");
        if side == "shell" {
            let (code, out, serr) = shell_run(
                home,
                &[destination.as_os_str()],
                "_overlay_write_private_line \"$2\" $'a\\tb'; code=$?; mode=$(stat -c '%a' \"$2\" 2>/dev/null || stat -f '%Lp' \"$2\" 2>/dev/null || echo NONE); printf 'rc=%s\\nmode=%s\\nbody=%s\\n' \"$code\" \"$mode\" \"$(cat \"$2\" 2>/dev/null || echo NONE)\"\n",
            );
            assert_eq!(code, 0, "harness exit");
            assert!(serr.is_empty(), "write stderr: {serr:?}");
            assert_eq!(
                String::from_utf8(out).expect("write dump"),
                "rc=0\nmode=600\nbody=a\tb\n",
                "shell private line"
            );
        } else {
            assert!(
                repos_overlays::write_private_line(&destination, "a\tb", euid, &tool),
                "rust private line"
            );
            let mode = dot::temp::file_mode(&destination)
                .map(|mode| format!("{:o}", mode & 0o777))
                .expect("mode");
            assert_eq!(mode, "600", "rust private mode");
            assert_eq!(
                std::fs::read_to_string(&destination).expect("body"),
                "a\tb\n",
                "rust private body"
            );
        }
    }
    stage(home, "taken.tsv", b"old\n");
    std::os::unix::fs::symlink("taken.tsv", home.join("linked.tsv")).expect("link");
    for name in ["taken.tsv", "linked.tsv"] {
        let destination = home.join(name);
        let (code, out, serr) = shell_run(
            home,
            &[destination.as_os_str()],
            "_overlay_write_private_line \"$2\" 'new'; printf 'rc=%s\\n' \"$?\"\n",
        );
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(serr.is_empty(), "refusal stderr: {serr:?}");
        assert_eq!(
            String::from_utf8(out).expect("refusal dump"),
            "rc=1\n",
            "shell refuses {name:?}"
        );
        assert!(
            !repos_overlays::write_private_line(&destination, "new", euid, &tool),
            "rust refuses {name:?}"
        );
        // Refusals preserve files and links exactly.
        if name == "linked.tsv" {
            assert_eq!(
                std::fs::read_link(&destination).expect("link intact"),
                Path::new("taken.tsv"),
                "refusal preserves link {name:?}"
            );
        } else {
            assert_eq!(
                std::fs::read(&destination).unwrap_or_default(),
                b"old\n",
                "refusal preserves file {name:?}"
            );
        }
    }
}

#[test]
fn private_directory_agrees() {
    let dir = TempDir::new("ovpub-privdir").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    std::fs::create_dir_all(home.join("locked")).expect("locked");
    std::fs::set_permissions(home.join("locked"), std::fs::Permissions::from_mode(0o700))
        .expect("chmod");
    std::fs::create_dir_all(home.join("open")).expect("open");
    std::fs::set_permissions(home.join("open"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    stage(home, "file", b"x\n");
    std::os::unix::fs::symlink("locked", home.join("linkdir")).expect("dir link");
    for (name, want) in [
        ("locked", 0),
        ("open", 1),
        ("file", 1),
        ("linkdir", 1),
        ("missing", 1),
    ] {
        let target = home.join(name);
        let (code, out, serr) = shell_run(
            home,
            &[target.as_os_str()],
            "if _overlay_private_directory \"$2\"; then code=0; else code=1; fi; printf 'rc=%s\\n' \"$code\"\n",
        );
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(serr.is_empty(), "privdir stderr: {serr:?}");
        let shell = String::from_utf8(out).expect("privdir dump");
        let rust_code = if repos_overlays::private_directory(&target, euid) {
            0
        } else {
            1
        };
        assert_eq!(rust_code, want, "rust expectation for {name:?}");
        assert_eq!(
            format!("rc={rust_code}\n"),
            shell,
            "private directory for {name:?}"
        );
    }
}
