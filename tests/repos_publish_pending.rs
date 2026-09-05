//! Differential parity tests for the pending/fallback publish layer
//! of `lib/dot/repos/overlays.sh`: authority manifest discovery,
//! authority loading, candidate appending, and the pending and
//! fallback-authority publishers plus the fallback target search.
//!
//! Every case runs the live shell function and its Rust twin on
//! identical fixtures and compares exit status, selection, and file
//! bytes.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_overlays;
use dot::test_support::TempDir;

/// Run one shell snippet with the publish runtime sourced (the
/// publishers share the reserved/xdg dependencies of the leaf
/// layer).
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

/// chmod a fixture to an exact mode.
fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod fixture");
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

// Normalize one side directory out of a dump so the shell and Rust
// twins (which run in `-shell` / `-rust` fixtures) compare by
// content, not by temp path.
fn scrub(dump: &str, root: &Path) -> String {
    dump.replace(&root.to_string_lossy().into_owned(), "{SIDE}")
}

/// Hermetic reserved-roots env mirrored into `DestinationInputs`.
/// `None` means unset on both sides (the shell falls back to `$HOME`
/// defaults, like the engine).
struct TestEnv {
    xdg_state_home: Option<String>,
    install_dir: Option<String>,
    state_dir: Option<String>,
    init_backup: Option<String>,
}

impl TestEnv {
    fn empty() -> Self {
        Self {
            xdg_state_home: None,
            install_dir: None,
            state_dir: None,
            init_backup: None,
        }
    }

    /// Export the env into a snippet whose `$HOME` is `side`.
    fn preamble(&self, side: &Path, overlays: &[String], manifest: &str, legacy: &str) -> String {
        let mut out = format!("export HOME={}; ", sq(&side.to_string_lossy()));
        out.push_str(&format!(
            "DOT_OVERLAY_MANIFEST={} DOT_OVERLAY_LEGACY_MANIFEST={} ",
            sq(manifest),
            sq(legacy),
        ));
        out.push_str("OVERLAYS=(");
        for entry in overlays {
            out.push_str(&sq(entry));
            out.push(' ');
        }
        out.push_str("); ");
        let mut exports = Vec::new();
        if let Some(dir) = &self.xdg_state_home {
            exports.push(format!("XDG_STATE_HOME={}", sq(dir)));
        }
        if let Some(dir) = &self.install_dir {
            exports.push(format!("SHDEPS_INSTALL_DIR={}", sq(dir)));
        }
        if let Some(dir) = &self.state_dir {
            exports.push(format!("SHDEPS_STATE_DIR={}", sq(dir)));
        }
        if let Some(dir) = &self.init_backup {
            exports.push(format!("DOT_INIT_BACKUP={}", sq(dir)));
        }
        if !exports.is_empty() {
            out.push_str("export ");
            out.push_str(&exports.join(" "));
            out.push_str("; ");
        }
        out.push('\n');
        out
    }

    /// Mirror the preamble env into Rust inputs.
    fn inputs(&self, side: &Path, overlays: &[String]) -> repos_overlays::DestinationInputs {
        let side_text = side.to_string_lossy().into_owned();
        repos_overlays::DestinationInputs {
            pwd: side_text.clone(),
            home: side_text,
            xdg_state_home: self.xdg_state_home.clone(),
            install_dir: self.install_dir.clone(),
            state_dir: self.state_dir.clone(),
            overlay_paths: overlays
                .iter()
                .filter_map(|entry| entry.split('|').nth(1).map(str::to_string))
                .collect(),
            init_backup: self.init_backup.clone(),
        }
    }
}

/// `name|path|url|conf|optional|sync` overlay record.
fn ov(name: &str, path: &str, sync: &str) -> String {
    format!("{name}|{path}|https://example.invalid/x|||{sync}")
}

/// Manifest locations for a fixture home: selected, legacy,
/// pending (derived exactly like the shell).
fn manifests(home: &Path) -> (String, String, String) {
    let manifest = home.join("manifest.tsv");
    let legacy = home.join("legacy.tsv");
    (
        manifest.to_string_lossy().into_owned(),
        legacy.to_string_lossy().into_owned(),
        format!("{}.pending", manifest.to_string_lossy()),
    )
}

/// A safe manifest record line.
fn record(rel: &str, owner: &str, target: &str) -> Vec<u8> {
    format!("{rel}\t{owner}\t{target}\n").into_bytes()
}

/// Export the manifest locations then dump `_overlay_authority_files`.
fn authority_snippet(manifest: &str, legacy: &str) -> String {
    format!(
        "DOT_OVERLAY_MANIFEST={} DOT_OVERLAY_LEGACY_MANIFEST={} _overlay_authority_files; code=$?; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"; for m in ${{OVERLAY_AUTHORITY_MANIFESTS[@]+\"${{OVERLAY_AUTHORITY_MANIFESTS[@]}}\"}}; do printf 'manifest=%s\\n' \"$m\"; done\n",
        sq(manifest),
        sq(legacy),
    )
}

#[test]
fn authority_files_agrees() {
    let dir = TempDir::new("ovpend-authority").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    // (setup): each case uses a fresh subdirectory as HOME so the
    // shell and Rust twins share identical fixtures.
    struct Case {
        name: &'static str,
        setup: fn(&Path, &str, &str),
    }
    fn none(_home: &Path, _manifest: &str, _legacy: &str) {}
    fn selected(home: &Path, manifest: &str, _legacy: &str) {
        let body = record("app.conf", "base", ".config/app.conf");
        let path = stage(home, "manifest.tsv", &body);
        chmod(&path, 0o600);
        let _ = manifest;
    }
    fn legacy_only(home: &Path, _manifest: &str, _legacy: &str) {
        let path = stage(home, "legacy.tsv", b"old.conf\tbase\n");
        chmod(&path, 0o600);
    }
    fn pending_only(home: &Path, manifest: &str, _legacy: &str) {
        let body = record("app.conf", "base", ".config/app.conf");
        let path = stage(home, "manifest.tsv.pending", &body);
        chmod(&path, 0o600);
        let _ = manifest;
    }
    fn unsafe_selected(home: &Path, _manifest: &str, _legacy: &str) {
        let path = stage(home, "manifest.tsv", b"app.conf\tbase\tgone\n");
        // Force group-readable bits regardless of ambient umask.
        chmod(&path, 0o644);
    }
    fn unsafe_pending(home: &Path, manifest: &str, _legacy: &str) {
        let good = stage(home, "manifest.tsv", &record("a", "base", "t"));
        chmod(&good, 0o600);
        stage(home, "manifest.tsv.pending", b"junk\n");
        let _ = manifest;
    }
    for case in [
        Case {
            name: "none",
            setup: none,
        },
        Case {
            name: "selected",
            setup: selected,
        },
        Case {
            name: "legacy",
            setup: legacy_only,
        },
        Case {
            name: "pending",
            setup: pending_only,
        },
        Case {
            name: "unsafe-selected",
            setup: unsafe_selected,
        },
        Case {
            name: "unsafe-pending",
            setup: unsafe_pending,
        },
    ] {
        let root = home.join(case.name);
        std::fs::create_dir_all(&root).expect("case dir");
        let (manifest, legacy, _pending) = manifests(&root);
        (case.setup)(&root, &manifest, &legacy);
        let (code, out, _serr) = shell_run(&root, &[], &authority_snippet(&manifest, &legacy));
        assert_eq!(code, 0, "harness exit for {}", case.name);
        let shell = String::from_utf8(out).expect("authority dump");
        let rust = match repos_overlays::authority_files(&manifest, &legacy, euid) {
            Ok(found) => {
                let mut dump = format!("rc=0\nreply={}\n", found.pending);
                for entry in &found.manifests {
                    dump.push_str(&format!("manifest={entry}\n"));
                }
                dump
            }
            Err(unsafe_path) => format!("rc=1\nreply={unsafe_path}\n"),
        };
        assert_eq!(rust, shell, "authority files for {}", case.name);
    }
}

#[test]
fn load_authority_agrees() {
    let dir = TempDir::new("ovpend-load").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    let env = TestEnv::empty();
    // Manifest holds a plain record plus a self-referential one
    // (`manifest.tsv` is authority: it names the manifest itself),
    // so the twins must agree on both selection and skipping.
    let body = [
        record("app.conf", "base", ".config/app.conf"),
        record("manifest.tsv", "base", ".config/manifest.tsv"),
    ]
    .concat();
    let manifest = stage(home, "manifest.tsv", &body);
    chmod(&manifest, 0o600);
    let (manifest_text, legacy_text, _pending) = manifests(home);
    let overlays: Vec<String> = Vec::new();
    let snippet = format!(
        "{}declare -A _overlay_authority_paths=() _overlay_authority_targets=(); if _overlay_load_authority; then code=0; else code=1; fi; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"; for k in \"${{!_overlay_authority_paths[@]}}\"; do printf 'path=%s\\n' \"$k\"; done | LC_ALL=C sort; for k in \"${{!_overlay_authority_targets[@]}}\"; do printf 'target=%s\\n' \"$k\"; done | LC_ALL=C sort\n",
        env.preamble(home, &overlays, &manifest_text, &legacy_text),
    );
    let (code, out, serr) = shell_run(home, &[], &snippet);
    assert_eq!(code, 0, "harness exit");
    assert!(serr.is_empty(), "load stderr: {serr:?}");
    let shell = String::from_utf8(out).expect("load dump");
    let inputs = env.inputs(home, &overlays);
    let mut cache = repos_overlays::AuthorityCache::disabled();
    let mut ctx = repos_overlays::AuthorityCtx {
        home: &inputs.home,
        manifest: &manifest_text,
        legacy_manifest: &legacy_text,
        inputs: &inputs,
        roots: None,
        cache: &mut cache,
        euid,
    };
    let rust = match repos_overlays::load_authority(&mut ctx) {
        Ok(data) => {
            let mut dump = String::from("rc=0\nreply=\n");
            let mut paths: Vec<&String> = data.paths.iter().collect();
            paths.sort();
            for path in paths {
                dump.push_str(&format!("path={path}\n"));
            }
            let mut targets: Vec<(&String, &String)> = data
                .targets
                .iter()
                .map(|(rel, target)| (rel, target))
                .collect();
            targets.sort();
            for (rel, target) in targets {
                dump.push_str(&format!("target={rel}\t{target}\n"));
            }
            dump
        }
        Err(unsafe_path) => format!("rc=1\nreply={unsafe_path}\n"),
    };
    // The shell leaves `$REPLY` at the pending path on success.
    let rust = rust.replacen(
        "rc=0\nreply=\n",
        &format!("rc=0\nreply={manifest_text}.pending\n"),
        1,
    );
    assert_eq!(rust, shell, "load authority");
}

#[test]
fn append_manifest_records_agrees() {
    let dir = TempDir::new("ovpend-append").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    let env = TestEnv::empty();
    // The mid-file failure pins incremental writes — earlier lines
    // stay in the destination; the empty source pins the no-touch
    // shell loop (rc 0, destination absent).
    let bodies: &[(&str, &[u8])] = &[
        (
            "ok",
            &[
                record("app.conf", "base", ".config/app.conf"),
                record("manifest.tsv", "base", ".config/manifest.tsv"),
            ]
            .concat(),
        ),
        (
            "mid-fail",
            &[
                record("app.conf", "base", ".config/app.conf"),
                b"junk\n".to_vec(),
            ]
            .concat(),
        ),
        ("empty", b""),
    ];
    for (name, body) in bodies {
        for side in ["shell", "rust"] {
            let root = home.join(format!("{name}-{side}"));
            std::fs::create_dir_all(&root).expect("side dir");
            let (manifest_text, legacy_text, _pending) = manifests(&root);
            let manifest = stage(&root, "manifest.tsv", &record("m", "base", "t"));
            chmod(&manifest, 0o600);
            let source = stage(&root, "source.tsv", body);
            let destination = root.join("dest.tsv");
            let overlays: Vec<String> = Vec::new();
            if side == "shell" {
                let snippet = format!(
                    "{}_overlay_append_manifest_records {} {}; code=$?; printf 'rc=%s\\n' \"$code\"; printf 'dest='; cat {} 2>/dev/null || true\n",
                    env.preamble(&root, &overlays, &manifest_text, &legacy_text),
                    sq(&source.to_string_lossy()),
                    sq(&destination.to_string_lossy()),
                    sq(&destination.to_string_lossy()),
                );
                let (code, out, serr) = shell_run(&root, &[], &snippet);
                assert_eq!(code, 0, "harness exit for {name:?}");
                assert!(serr.is_empty(), "append stderr for {name:?}: {serr:?}");
                let shell = String::from_utf8(out).expect("append dump");
                std::fs::write(home.join(format!("{name}.shell.out")), shell)
                    .expect("stash shell dump");
            } else {
                let inputs = env.inputs(&root, &overlays);
                let mut cache = repos_overlays::AuthorityCache::disabled();
                let mut ctx = repos_overlays::AuthorityCtx {
                    home: &inputs.home,
                    manifest: &manifest_text,
                    legacy_manifest: &legacy_text,
                    inputs: &inputs,
                    roots: None,
                    cache: &mut cache,
                    euid,
                };
                let ok = repos_overlays::append_manifest_records(&source, &destination, &mut ctx);
                let mut rust = format!("rc={}\n", if ok { 0 } else { 1 });
                rust.push_str("dest=");
                rust.push_str(&std::fs::read_to_string(&destination).unwrap_or_default());
                let shell = std::fs::read_to_string(home.join(format!("{name}.shell.out")))
                    .expect("shell dump");
                assert_eq!(rust, shell, "append manifest records for {name:?}");
            }
        }
    }
    // A missing source fails without touching the destination.
    let root = home.join("missing-rust");
    std::fs::create_dir_all(&root).expect("side dir");
    let (manifest_text, legacy_text, _pending) = manifests(&root);
    let overlays: Vec<String> = Vec::new();
    let inputs = env.inputs(&root, &overlays);
    let mut cache = repos_overlays::AuthorityCache::disabled();
    let mut ctx = repos_overlays::AuthorityCtx {
        home: &inputs.home,
        manifest: &manifest_text,
        legacy_manifest: &legacy_text,
        inputs: &inputs,
        roots: None,
        cache: &mut cache,
        euid,
    };
    let destination = root.join("dest.tsv");
    assert!(
        !repos_overlays::append_manifest_records(&root.join("nope.tsv"), &destination, &mut ctx),
        "missing source fails"
    );
    assert!(!destination.exists(), "missing source leaves dest alone");
    let snippet = format!(
        "{}_overlay_append_manifest_records {} {}; printf 'rc=%s\\n' \"$?\"\n",
        env.preamble(&root, &overlays, &manifest_text, &legacy_text),
        sq(&root.join("nope.tsv").to_string_lossy()),
        sq(&destination.to_string_lossy()),
    );
    let (code, out, _serr) = shell_run(&root, &[], &snippet);
    assert_eq!(code, 0, "harness exit for missing source");
    assert_eq!(
        String::from_utf8(out).expect("missing dump"),
        "rc=1\n",
        "shell missing source"
    );
}

/// Build one overlay source tree per side: `$root/ov/$name/home`
/// holds `files`, and the NUL inventory lists `extra` raw entries
/// (usually the same shipped paths, plus boundary entries).
fn stage_overlay(root: &Path, name: &str, files: &[&str], extra: &[String]) -> (String, PathBuf) {
    let base = root.join("ov").join(name);
    for file in files {
        stage(&base, &format!("home/{file}"), b"x\n");
    }
    let mut inventory = Vec::new();
    for file in files {
        inventory.extend_from_slice(base.join("home").join(file).to_string_lossy().as_bytes());
        inventory.push(0);
    }
    for entry in extra {
        inventory.extend_from_slice(entry.as_bytes());
        inventory.push(0);
    }
    let inventory_path = stage(&base, "inventory.nul", &inventory);
    (base.to_string_lossy().into_owned(), inventory_path)
}

#[test]
fn append_candidates_agrees() {
    let dir = TempDir::new("ovpend-candidates").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    let env = TestEnv::empty();
    // (name, sync, extra inventory entries): the authority entry
    // pins whole-function failure, the stray entry pins
    // prefix-strip parsing failure, and bogus sync pins the
    // unpublishable-source refusal.
    let cases: &[(&str, &str)] = &[("none-ok", "none"), ("git-ok", "git"), ("bogus", "bogus")];
    for (name, sync) in cases {
        for side in ["shell", "rust"] {
            let root = home.join(format!("{name}-{side}"));
            std::fs::create_dir_all(&root).expect("side dir");
            let (manifest_text, legacy_text, _pending) = manifests(&root);
            let manifest = stage(&root, "manifest.tsv", &record("m", "base", "t"));
            chmod(&manifest, 0o600);
            let (ov_path, inventory) =
                stage_overlay(&root, "web", &["app.conf", "other.conf"], &[]);
            let destination = root.join("dest.tsv");
            let overlays: Vec<String> = Vec::new();
            if side == "shell" {
                let snippet = format!(
                    "{}_overlay_append_candidates {} {} {} {} {}; code=$?; printf 'rc=%s\\n' \"$code\"; printf 'dest='; cat {} 2>/dev/null || true\n",
                    env.preamble(&root, &overlays, &manifest_text, &legacy_text),
                    sq(&destination.to_string_lossy()),
                    sq("web"),
                    sq(&ov_path),
                    sq(&inventory.to_string_lossy()),
                    sq(sync),
                    sq(&destination.to_string_lossy()),
                );
                let (code, out, serr) = shell_run(&root, &[], &snippet);
                assert_eq!(code, 0, "harness exit for {name:?}");
                assert!(serr.is_empty(), "candidates stderr for {name:?}: {serr:?}");
                let shell = String::from_utf8(out).expect("candidates dump");
                std::fs::write(home.join(format!("{name}.shell.out")), scrub(&shell, &root))
                    .expect("stash shell dump");
            } else {
                let inputs = env.inputs(&root, &overlays);
                let mut cache = repos_overlays::AuthorityCache::disabled();
                let mut ctx = repos_overlays::AuthorityCtx {
                    home: &inputs.home,
                    manifest: &manifest_text,
                    legacy_manifest: &legacy_text,
                    inputs: &inputs,
                    roots: None,
                    cache: &mut cache,
                    euid,
                };
                let ok = repos_overlays::append_candidates(
                    &destination,
                    "web",
                    &ov_path,
                    &inventory,
                    Some(sync),
                    &mut ctx,
                );
                let mut rust = format!("rc={}\n", if ok { 0 } else { 1 });
                rust.push_str("dest=");
                rust.push_str(&std::fs::read_to_string(&destination).unwrap_or_default());
                let shell = std::fs::read_to_string(home.join(format!("{name}.shell.out")))
                    .expect("shell dump");
                assert_eq!(scrub(&rust, &root), shell, "append candidates for {name:?}");
            }
        }
    }
    // Boundary inventories, Rust side only for setup brevity, each
    // mirrored by the shell on the same side layout.
    for (name, files, extra) in [
        ("authority", &["app.conf", "manifest.tsv"][..], &[][..]),
        (
            "stray",
            &["app.conf"][..],
            &["/elsewhere/app.conf".to_string()][..],
        ),
    ] {
        for side in ["shell", "rust"] {
            let root = home.join(format!("boundary-{name}-{side}"));
            std::fs::create_dir_all(&root).expect("side dir");
            let (manifest_text, legacy_text, _pending) = manifests(&root);
            let manifest = stage(&root, "manifest.tsv", &record("m", "base", "t"));
            chmod(&manifest, 0o600);
            let (ov_path, inventory) = stage_overlay(&root, "web", files, extra);
            let destination = root.join("dest.tsv");
            let overlays: Vec<String> = Vec::new();
            if side == "shell" {
                let snippet = format!(
                    "{}_overlay_append_candidates {} {} {} {} none; code=$?; printf 'rc=%s\\n' \"$code\"; printf 'dest='; cat {} 2>/dev/null || true\n",
                    env.preamble(&root, &overlays, &manifest_text, &legacy_text),
                    sq(&destination.to_string_lossy()),
                    sq("web"),
                    sq(&ov_path),
                    sq(&inventory.to_string_lossy()),
                    sq(&destination.to_string_lossy()),
                );
                let (code, out, _serr) = shell_run(&root, &[], &snippet);
                assert_eq!(code, 0, "harness exit for boundary {name:?}");
                let shell = String::from_utf8(out).expect("boundary dump");
                std::fs::write(
                    home.join(format!("boundary-{name}.shell.out")),
                    scrub(&shell, &root),
                )
                .expect("stash shell dump");
            } else {
                let inputs = env.inputs(&root, &overlays);
                let mut cache = repos_overlays::AuthorityCache::disabled();
                let mut ctx = repos_overlays::AuthorityCtx {
                    home: &inputs.home,
                    manifest: &manifest_text,
                    legacy_manifest: &legacy_text,
                    inputs: &inputs,
                    roots: None,
                    cache: &mut cache,
                    euid,
                };
                let ok = repos_overlays::append_candidates(
                    &destination,
                    "web",
                    &ov_path,
                    &inventory,
                    Some("none"),
                    &mut ctx,
                );
                let mut rust = format!("rc={}\n", if ok { 0 } else { 1 });
                rust.push_str("dest=");
                rust.push_str(&std::fs::read_to_string(&destination).unwrap_or_default());
                let shell =
                    std::fs::read_to_string(home.join(format!("boundary-{name}.shell.out")))
                        .expect("shell dump");
                assert_eq!(
                    scrub(&rust, &root),
                    shell,
                    "append candidates boundary {name:?}"
                );
            }
        }
    }
    // A missing or symlinked inventory fails outright.
    let root = home.join("inventory-guard");
    std::fs::create_dir_all(&root).expect("side dir");
    let (manifest_text, legacy_text, _pending) = manifests(&root);
    let overlays: Vec<String> = Vec::new();
    let inputs = env.inputs(&root, &overlays);
    let (ov_path, _inventory) = stage_overlay(&root, "web", &["app.conf"], &[]);
    let destination = root.join("dest.tsv");
    std::os::unix::fs::symlink("inventory.nul", root.join("ov/web/link.nul")).expect("link");
    for inventory in [
        root.join("ov/web/missing.nul"),
        root.join("ov/web/link.nul"),
    ] {
        let mut cache = repos_overlays::AuthorityCache::disabled();
        let mut ctx = repos_overlays::AuthorityCtx {
            home: &inputs.home,
            manifest: &manifest_text,
            legacy_manifest: &legacy_text,
            inputs: &inputs,
            roots: None,
            cache: &mut cache,
            euid,
        };
        assert!(
            !repos_overlays::append_candidates(
                &destination,
                "web",
                &ov_path,
                &inventory,
                Some("none"),
                &mut ctx
            ),
            "rust refuses inventory {}",
            inventory.display()
        );
        let snippet = format!(
            "{}_overlay_append_candidates {} web {} {} none; printf 'rc=%s\\n' \"$?\"\n",
            env.preamble(&root, &overlays, &manifest_text, &legacy_text),
            sq(&destination.to_string_lossy()),
            sq(&ov_path),
            sq(&inventory.to_string_lossy()),
        );
        let (code, out, _serr) = shell_run(&root, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {}", inventory.display());
        assert_eq!(
            String::from_utf8(out).expect("guard dump"),
            "rc=1\n",
            "shell refuses {}",
            inventory.display()
        );
    }
}

/// Dump a pending publish: rc, pending path, mode, and bytes.
fn dump_pending(pending: &Path, code: i32) -> String {
    use std::os::unix::fs::MetadataExt as _;
    let mut dump = format!("rc={code}\npending={}\n", pending.display());
    match std::fs::symlink_metadata(pending) {
        Ok(meta) => {
            dump.push_str(&format!("mode={:o}\n", meta.mode() & 0o777));
            dump.push_str("body=");
            dump.push_str(&std::fs::read_to_string(pending).unwrap_or_default());
        }
        Err(_) => dump.push_str("mode=NONE\nbody=NONE\n"),
    }
    dump
}

#[test]
fn publish_pending_agrees() {
    let dir = TempDir::new("ovpend-publish").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let env = TestEnv::empty();
    // (name, manifest body or None for absent, pre-existing pending
    // body or None, overlay sync): fresh create, replace-over-safe,
    // unsafe refusal, inventory-less skip, and candidate failure
    // (which must leave no temp file and no pending behind).
    struct Setup {
        manifest: Option<Vec<u8>>,
        pending: Option<Vec<u8>>,
        sync: &'static str,
        tracked: bool,
    }
    let cases: &[(&str, Setup)] = &[
        (
            "fresh",
            Setup {
                manifest: Some(record("app.conf", "base", ".config/app.conf")),
                pending: None,
                sync: "none",
                tracked: true,
            },
        ),
        (
            "replace",
            Setup {
                manifest: Some(record("old.conf", "base", ".config/old.conf")),
                pending: Some(record("stale.conf", "base", ".config/stale.conf")),
                sync: "none",
                tracked: true,
            },
        ),
        (
            "unsafe",
            Setup {
                manifest: Some(b"app.conf\tbase\tgone\n".to_vec()),
                pending: None,
                sync: "none",
                tracked: true,
            },
        ),
        (
            "untracked",
            Setup {
                manifest: Some(record("app.conf", "base", ".config/app.conf")),
                pending: None,
                sync: "none",
                tracked: false,
            },
        ),
        (
            "candidate-fails",
            Setup {
                manifest: Some(record("app.conf", "base", ".config/app.conf")),
                pending: None,
                sync: "bogus",
                tracked: true,
            },
        ),
    ];
    for (name, setup) in cases {
        for side in ["shell", "rust"] {
            let root = home.join(format!("{name}-{side}"));
            std::fs::create_dir_all(&root).expect("side dir");
            let (manifest_text, legacy_text, pending_text) = manifests(&root);
            if let Some(body) = &setup.manifest {
                let manifest = stage(&root, "manifest.tsv", body);
                // The unsafe case pins the mode gate with a
                // parseable record, so force group-readable bits
                // regardless of the ambient umask.
                chmod(&manifest, if *name == "unsafe" { 0o644 } else { 0o600 });
            }
            if let Some(body) = &setup.pending {
                let pending = stage(&root, "manifest.tsv.pending", body);
                chmod(&pending, 0o600);
            }
            let (ov_path, inventory) = stage_overlay(&root, "web", &["app.conf"], &[]);
            let overlays = vec![ov("web", &ov_path, setup.sync)];
            let mut inventories = std::collections::HashMap::new();
            if setup.tracked {
                inventories.insert("web".to_string(), inventory.clone());
            }
            if side == "shell" {
                let inv_map = if setup.tracked {
                    format!(
                        "declare -A _overlay_inventory_files=([web]={}); ",
                        sq(&inventory.to_string_lossy())
                    )
                } else {
                    String::from("declare -A _overlay_inventory_files=(); ")
                };
                let snippet = format!(
                    "{}{}_overlay_publish_pending; code=$?; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$REPLY\"; p={}; if [[ -e $p || -L $p ]]; then printf 'mode=%s\\n' \"$(stat -c '%a' \"$p\" 2>/dev/null || stat -f '%Lp' \"$p\" 2>/dev/null || echo NONE)\"; printf 'body='; cat \"$p\" 2>/dev/null || true; else printf 'mode=NONE\\nbody=NONE\\n'; fi\n",
                    env.preamble(&root, &overlays, &manifest_text, &legacy_text),
                    inv_map,
                    sq(&pending_text),
                );
                let (code, out, _serr) = shell_run(&root, &[], &snippet);
                assert_eq!(code, 0, "harness exit for {name:?}");
                let shell = String::from_utf8(out).expect("publish dump");
                std::fs::write(home.join(format!("{name}.shell.out")), scrub(&shell, &root))
                    .expect("stash shell dump");
            } else {
                let inputs = env.inputs(&root, &overlays);
                let mut cache = repos_overlays::AuthorityCache::disabled();
                let mut ctx = repos_overlays::AuthorityCtx {
                    home: &inputs.home,
                    manifest: &manifest_text,
                    legacy_manifest: &legacy_text,
                    inputs: &inputs,
                    roots: None,
                    cache: &mut cache,
                    euid,
                };
                let result =
                    repos_overlays::publish_pending(&mut ctx, euid, &overlays, &inventories, &tool);
                let shell = std::fs::read_to_string(home.join(format!("{name}.shell.out")))
                    .expect("shell dump");
                let shell_lines: Vec<&str> = shell.lines().collect();
                assert!(shell_lines.len() >= 2, "shell dump shape for {name:?}");
                let shell_rc: i32 = shell_lines[0]
                    .strip_prefix("rc=")
                    .expect("rc")
                    .parse()
                    .expect("rc number");
                match result {
                    Some(pending) => {
                        assert_eq!(shell_rc, 0, "shell rc for {name:?}");
                        // Success pins `$REPLY` at the pending path.
                        assert_eq!(
                            shell_lines[1],
                            format!("reply={pending_text}")
                                .replace(&root.to_string_lossy().into_owned(), "{SIDE}"),
                            "shell reply for {name:?}"
                        );
                        let rust = dump_pending(Path::new(&pending), 0);
                        let rust = scrub(
                            &format!(
                                "rc=0\nreply={}\n{}",
                                pending,
                                rust.lines().skip(2).collect::<Vec<_>>().join("\n") + "\n"
                            ),
                            &root,
                        );
                        assert_eq!(rust, shell, "publish pending for {name:?}");
                    }
                    None => {
                        assert_eq!(shell_rc, 1, "shell rc for {name:?}");
                        // Failure file effects compare at the
                        // deterministic pending path; the shell
                        // `$REPLY` residue is caller-unread helper
                        // noise (both in-engine callers return
                        // without touching it).
                        let rust_state = dump_pending(Path::new(&pending_text), 1);
                        let rust_state: Vec<&str> = rust_state.lines().skip(2).collect();
                        assert_eq!(
                            shell_lines[2..],
                            rust_state,
                            "publish failure effects for {name:?}"
                        );
                    }
                }
                // Failures leave no temp file behind.
                let leftovers: Vec<_> = std::fs::read_dir(&root)
                    .expect("scan side")
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.contains(".tmp."))
                    .collect();
                assert!(
                    leftovers.is_empty(),
                    "temp leftovers for {name:?}: {leftovers:?}"
                );
            }
        }
    }
}

#[test]
fn publish_fallback_authority_agrees() {
    let dir = TempDir::new("ovpend-fallback").expect("fixture dir");
    let home = dir.path();
    let euid = dot::temp::current_uid().expect("current uid");
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    let env = TestEnv::empty();
    // (name, selected body, pending body or None): exact-hit
    // no-op, fresh append, append beside a safe pending, and the
    // unsafe-manifest refusal.
    type Case = (&'static str, Vec<u8>, Option<Vec<u8>>);
    let cases: &[Case] = &[
        ("hit", record("app.conf", "web", ".config/app.conf"), None),
        (
            "fresh",
            record("other.conf", "web", ".config/other.conf"),
            None,
        ),
        (
            "beside",
            record("other.conf", "web", ".config/other.conf"),
            Some(record("stale.conf", "base", ".config/stale.conf")),
        ),
        ("unsafe", b"junk\n".to_vec(), None),
    ];
    for (name, selected, pending_body) in cases {
        for side in ["shell", "rust"] {
            let root = home.join(format!("{name}-{side}"));
            std::fs::create_dir_all(&root).expect("side dir");
            let (manifest_text, legacy_text, pending_text) = manifests(&root);
            let manifest = stage(&root, "manifest.tsv", selected);
            if *name != "unsafe" {
                chmod(&manifest, 0o600);
            }
            if let Some(body) = pending_body {
                let pending = stage(&root, "manifest.tsv.pending", body);
                chmod(&pending, 0o600);
            }
            let before = std::fs::read(&manifest).unwrap_or_default();
            let overlays: Vec<String> = Vec::new();
            if side == "shell" {
                let snippet = format!(
                    "{}_overlay_publish_fallback_authority app.conf web .config/app.conf; code=$?; printf 'rc=%s\\n' \"$code\"; printf 'selected='; cat {} 2>/dev/null; printf 'pending='; cat {} 2>/dev/null || true\n",
                    env.preamble(&root, &overlays, &manifest_text, &legacy_text),
                    sq(&manifest_text),
                    sq(&pending_text),
                );
                let (code, out, _serr) = shell_run(&root, &[], &snippet);
                assert_eq!(code, 0, "harness exit for {name:?}");
                let shell = String::from_utf8(out).expect("fallback dump");
                std::fs::write(home.join(format!("{name}.shell.out")), shell)
                    .expect("stash shell dump");
            } else {
                let inputs = env.inputs(&root, &overlays);
                let mut cache = repos_overlays::AuthorityCache::disabled();
                let mut ctx = repos_overlays::AuthorityCtx {
                    home: &inputs.home,
                    manifest: &manifest_text,
                    legacy_manifest: &legacy_text,
                    inputs: &inputs,
                    roots: None,
                    cache: &mut cache,
                    euid,
                };
                let ok = repos_overlays::publish_fallback_authority(
                    "app.conf",
                    "web",
                    ".config/app.conf",
                    &mut ctx,
                    euid,
                    &tool,
                );
                let mut rust = format!("rc={}\n", if ok { 0 } else { 1 });
                rust.push_str("selected=");
                rust.push_str(&String::from_utf8_lossy(
                    &std::fs::read(&manifest).unwrap_or_default(),
                ));
                rust.push_str("pending=");
                rust.push_str(&String::from_utf8_lossy(
                    &std::fs::read(Path::new(&pending_text)).unwrap_or_default(),
                ));
                let shell = std::fs::read_to_string(home.join(format!("{name}.shell.out")))
                    .expect("shell dump");
                assert_eq!(rust, shell, "publish fallback for {name:?}");
                if *name == "hit" {
                    assert_eq!(
                        std::fs::read(&manifest).unwrap_or_default(),
                        before,
                        "exact hit leaves the manifest alone"
                    );
                }
            }
        }
    }
}

#[test]
fn active_fallback_target_agrees() {
    let dir = TempDir::new("ovpend-activefb").expect("fixture dir");
    let home = dir.path();
    // Two overlays ship the same rel with different source roots
    // (sync none, so targets differ); a third ships nothing; a
    // fourth is unpublishable. The last publishable non-excluded
    // candidate wins.
    for name in ["alpha", "beta", "gamma", "delta"] {
        let files: &[&str] = match name {
            "alpha" | "beta" => &["app.conf"],
            "delta" => &["app.conf"],
            _ => &["other.conf"],
        };
        stage_overlay(home, name, files, &[]);
    }
    let entry =
        |name: &str, sync: &str| ov(name, &home.join("ov").join(name).to_string_lossy(), sync);
    let overlays = vec![
        entry("alpha", "none"),
        entry("beta", "none"),
        entry("gamma", "none"),
        entry("delta", "bogus"),
    ];
    let beta_target = format!("{}/ov/beta/home/app.conf", home.to_string_lossy());
    let alpha_target = format!("{}/ov/alpha/home/app.conf", home.to_string_lossy());
    for (excluded, want) in [
        ("", Some((beta_target.clone(), "beta".to_string()))),
        (
            beta_target.as_str(),
            Some((alpha_target.clone(), "alpha".to_string())),
        ),
        ("unrelated", Some((beta_target.clone(), "beta".to_string()))),
    ] {
        let active: Vec<String> = overlays.clone();
        let mut preamble = String::from("ACTIVE_OVERLAYS=(");
        for item in &active {
            preamble.push_str(&sq(item));
            preamble.push(' ');
        }
        preamble.push_str("); ");
        let snippet = format!(
            "{preamble}if _overlay_active_fallback_target app.conf {}; then code=0; else code=1; fi; printf 'rc=%s\\nreply=%s\\nowner=%s\\n' \"$code\" \"$REPLY\" \"$REPLY_OWNER\"\n",
            sq(excluded),
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for excluded={excluded:?}");
        assert!(serr.is_empty(), "fallback stderr: {serr:?}");
        let shell = String::from_utf8(out).expect("fallback dump");
        let rust = match repos_overlays::active_fallback_target("app.conf", excluded, &active) {
            Some((target, owner)) => format!("rc=0\nreply={target}\nowner={owner}\n"),
            None => String::from("rc=1\nreply=\nowner=\n"),
        };
        assert_eq!(rust, shell, "active fallback for excluded={excluded:?}");
        assert_eq!(
            repos_overlays::active_fallback_target("app.conf", excluded, &active),
            want,
            "rust expectation for excluded={excluded:?}"
        );
    }
    // Nothing ships the rel: both sides fail. The delta-only list
    // pins the unpublishable-source skip.
    for (name, active) in [
        ("missing", overlays.clone()),
        ("unpublishable", vec![entry("delta", "bogus")]),
    ] {
        let rel = if name == "missing" {
            "absent.conf"
        } else {
            "app.conf"
        };
        let mut preamble = String::from("ACTIVE_OVERLAYS=(");
        for item in &active {
            preamble.push_str(&sq(item));
            preamble.push(' ');
        }
        preamble.push_str("); ");
        let snippet = format!(
            "{preamble}if _overlay_active_fallback_target {rel} ''; then code=0; else code=1; fi; printf 'rc=%s\\n' \"$code\"\n",
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(serr.is_empty(), "missing stderr: {serr:?}");
        assert_eq!(
            String::from_utf8(out).expect("missing dump"),
            "rc=1\n",
            "shell {name:?}"
        );
        assert_eq!(
            repos_overlays::active_fallback_target(rel, "", &active),
            None,
            "rust {name:?}"
        );
    }
}
