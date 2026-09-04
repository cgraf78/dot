//! Differential parity tests for the quarantine chapter of
//! `lib/dot/repos/overlays.sh`: the destination context and the
//! quarantine orchestrator, against the live shell.
//!
//! Separate binary because this chapter needs a richer shell runtime
//! than the manifest/identity tests: `reserved.sh` (physical
//! candidates, reserved roots) plus `public/xdg.sh` (state-home
//! lookup) alongside `repos/overlays.sh`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_overlays;
use dot::test_support::TempDir;

/// Run one shell snippet with the quarantine runtime sourced.
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

/// Hermetic env for the reserved-roots inventory, mirrored into
/// `DestinationInputs` on the Rust side. `None` means unset on both
/// sides (the shell falls back to `$HOME` defaults, like the engine).
struct TestEnv {
    xdg_state_home: Option<String>,
    install_dir: Option<String>,
    state_dir: Option<String>,
    init_backup: Option<String>,
}

/// Preamble shared by the destination/quarantine snippets: rollback
/// snapshot arrays, OVERLAYS records, and the reserved-roots env.
fn quarantine_preamble(
    home: &Path,
    paths: &[String],
    targets: &[String],
    overlays: &[String],
    env: &TestEnv,
) -> String {
    // The harness HOME is the shared parent; each side exports its
    // own root so `$HOME/rel` resolves inside its fixture.
    let mut out = format!("export HOME={}; ", sq(&home.to_string_lossy()));
    out.push_str("DOT_OVERLAY_ROLLBACK_PATHS=(");
    for rel in paths {
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
    out.push_str("); ");
    let mut exports = Vec::new();
    if let Some(dir) = &env.xdg_state_home {
        exports.push(format!("XDG_STATE_HOME={}", sq(dir)));
    }
    if let Some(dir) = &env.install_dir {
        exports.push(format!("SHDEPS_INSTALL_DIR={}", sq(dir)));
    }
    if let Some(dir) = &env.state_dir {
        exports.push(format!("SHDEPS_STATE_DIR={}", sq(dir)));
    }
    if let Some(dir) = &env.init_backup {
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
fn test_inputs(
    home: &Path,
    overlays: &[String],
    env: &TestEnv,
) -> repos_overlays::DestinationInputs {
    let home_text = home.to_string_lossy().into_owned();
    repos_overlays::DestinationInputs {
        pwd: home_text.clone(),
        home: home_text,
        xdg_state_home: env.xdg_state_home.clone(),
        install_dir: env.install_dir.clone(),
        state_dir: env.state_dir.clone(),
        overlay_paths: overlays
            .iter()
            .filter_map(|entry| entry.split('|').nth(1).map(str::to_string))
            .collect(),
        init_backup: env.init_backup.clone(),
    }
}

/// Hermetic env pointing every inventory root under the fixture.
fn fixture_env(home: &Path) -> (TestEnv, String) {
    let home_text = home.to_string_lossy().into_owned();
    (
        TestEnv {
            xdg_state_home: Some(format!("{home_text}/xdg-state")),
            install_dir: Some(format!("{home_text}/install")),
            state_dir: Some(format!("{home_text}/shdeps")),
            init_backup: None,
        },
        home_text,
    )
}

#[test]
fn destination_context_agrees() {
    for rel in ["sub/anchor", "nodir/deep/x", ".dotfiles-evil/x"] {
        let dir = TempDir::new("ovlink-destctx").expect("fixture dir");
        let home = dir.path();
        stage(home, "sub/anchor-target", b"t\n");
        std::os::unix::fs::symlink("anchor-target", home.join("sub/anchor")).expect("symlink");
        let (env_full, _) = fixture_env(home);
        let overlay_records: Vec<String> = vec![];
        let snippet = format!(
            "{}if _overlay_destination_context \"$HOME/{rel}\"; then printf 'rc=0\\nphysical=%s\\nparent=%s\\nidentity=%s\\n' \"$OVERLAY_PHYSICAL_DESTINATION\" \"$OVERLAY_PHYSICAL_PARENT\" \"$OVERLAY_PARENT_IDENTITY\"; else printf 'rc=1\\n'; fi\n",
            quarantine_preamble(home, &[], &[], &overlay_records, &env_full),
        );
        let (code, out, serr) = shell_run(home, &[], &snippet);
        assert_eq!(code, 0, "harness exit for {rel:?}");
        assert!(serr.is_empty(), "context stderr for {rel:?}: {serr:?}");
        let shell = String::from_utf8(out).expect("context dump");
        let inputs = test_inputs(home, &overlay_records, &env_full);
        let rust = match repos_overlays::destination_context(rel, &inputs) {
            Some(ctx) => format!(
                "rc=0\nphysical={}\nparent={}\nidentity={}\n",
                ctx.physical.display(),
                ctx.parent.display(),
                ctx.parent_identity
            ),
            None => "rc=1\n".to_string(),
        };
        assert_eq!(rust, shell, "destination context for {rel:?}");
    }
}

/// Build one managed side under `root`: `sub/anchor` linking the
/// absolute target file. Returns `(rel, target)`; targets differ
/// per side, so each engine gets its own snapshot.
fn managed_side(root: &Path) -> (String, String) {
    std::fs::create_dir_all(root.join("sub")).expect("subdir");
    let target = stage(root, "target.txt", b"managed\n")
        .to_string_lossy()
        .into_owned();
    std::os::unix::fs::symlink(&target, root.join("sub/anchor")).expect("managed link");
    ("sub/anchor".to_string(), target)
}

/// Reshape one side for a refusal label: returns `(rel, snapshot)`.
/// The leaf starts managed; refusal cases point it elsewhere.
fn case_setup(root: &Path, label: &str) -> (String, (Vec<String>, Vec<String>)) {
    let (mut rel, target) = managed_side(root);
    let mut snapshot = (vec![rel.clone()], vec![target.clone()]);
    match label {
        "unknown-rel" => {
            snapshot = (vec!["other/path".to_string()], vec![target]);
        }
        "ragged-snapshot" => {
            snapshot = (vec!["sub/anchor".to_string()], vec![]);
        }
        "regular-file" => {
            std::fs::remove_file(root.join(&rel)).expect("unlink");
            stage(root, &rel, b"user file\n");
        }
        "wrong-target" => {
            std::fs::remove_file(root.join(&rel)).expect("unlink");
            std::os::unix::fs::symlink("elsewhere", root.join(&rel)).expect("rel link");
        }
        "reserved-rel" => {
            rel = ".dotfiles-evil/x".to_string();
            snapshot = (vec![rel.clone()], vec![target]);
        }
        _ => {}
    }
    (rel, snapshot)
}

/// Env plus overlay records for one side; the configured-env label
/// exercises non-default inventory roots on both engines.
fn case_env(home_text: &str, label: &str) -> (TestEnv, Vec<String>) {
    (
        TestEnv {
            xdg_state_home: Some(format!("{home_text}/xdg-state")),
            install_dir: Some(format!("{home_text}/install")),
            state_dir: Some(format!("{home_text}/shdeps")),
            init_backup: if label == "configured-env" {
                Some(format!("{home_text}/backup"))
            } else {
                None
            },
        },
        if label == "configured-env" {
            vec![format!("web|{home_text}/ov|x|||git")]
        } else {
            vec![]
        },
    )
}

fn quarantine_inputs(
    home: &Path,
    snapshot: (Vec<String>, Vec<String>),
    overlays: &[String],
    env: &TestEnv,
    tool: &dot::temp::MoveTool,
) -> repos_overlays::QuarantineInputs {
    repos_overlays::QuarantineInputs {
        snapshot: repos_overlays::RollbackSnapshot {
            paths: snapshot.0,
            targets: snapshot.1,
        },
        context: test_inputs(home, overlays, env),
        tool: tool.clone(),
        source_root: home.to_path_buf(),
    }
}

/// Shell aftermath probe for the refused quarantine cases: `rc`,
/// the physical leaf state, and whether a stage directory leaked
/// into the physical parent (`$2` rel, `$3` physical, `$4` parent).
fn refused_probe() -> &'static str {
    "_overlay_quarantine_rollback_link \"$2\"; code=$?; state=absent; \
     if [ -L \"$3\" ]; then state=\"link:$(readlink \"$3\")\"; \
     elif [ -f \"$3\" ]; then state=file; \
     elif [ -e \"$3\" ]; then state=other; fi; \
     leaked=no; \
     for entry in \"$4\"/.*.dot-overlay-adopt.*; do \
       [ -e \"$entry\" ] && leaked=yes; \
     done; \
     printf 'rc=%s\\nstate=%s\\nleaked=%s\\n' \"$code\" \"$state\" \"$leaked\"\n"
}

fn refused_rust(code: i32, physical: &Path, parent: &Path) -> String {
    let state = match std::fs::symlink_metadata(physical) {
        Err(_) => "absent".to_string(),
        Ok(meta) if meta.file_type().is_symlink() => format!(
            "link:{}",
            std::fs::read_link(physical)
                .map(|target| target.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        Ok(meta) if meta.is_file() => "file".to_string(),
        Ok(_) => "other".to_string(),
    };
    let leaked = std::fs::read_dir(parent).is_ok_and(|entries| {
        entries.filter_map(|entry| entry.ok()).any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".dot-overlay-adopt.")
        })
    });
    format!(
        "rc={code}\nstate={state}\nleaked={}\n",
        if leaked { "yes" } else { "no" }
    )
}

#[test]
fn quarantine_rollback_link_agrees() {
    let mut moves = dot::temp::MoveCache::default();
    let tool = moves.tool().expect("move tool");
    for label in [
        "happy",
        "unknown-rel",
        "ragged-snapshot",
        "regular-file",
        "wrong-target",
        "reserved-rel",
        "configured-env",
    ] {
        let dir = TempDir::new("ovlink-quarantine").expect("fixture dir");
        let home = dir.path();
        // Shell and Rust each work their own identical fixture:
        // quarantine mutates, and inode-bound identities only
        // compare within one side.
        let shell_root = home.join("shell");
        let rust_root = home.join("rust");
        let (srel, ssnapshot) = case_setup(&shell_root, label);
        let (rrel, rsnapshot) = case_setup(&rust_root, label);
        assert_eq!(srel, rrel, "both sides test the same rel");
        let shell_text = shell_root.to_string_lossy().into_owned();
        let rust_text = rust_root.to_string_lossy().into_owned();
        let (senv, soverlays) = case_env(&shell_text, label);
        let (renv, roverlays) = case_env(&rust_text, label);
        let physical = rust_root.join(&rrel);
        let parent = physical.parent().expect("parent").to_path_buf();
        let inputs = quarantine_inputs(&rust_root, rsnapshot, &roverlays, &renv, &tool);
        if label == "happy" || label == "configured-env" {
            let base = physical
                .file_name()
                .expect("base")
                .to_string_lossy()
                .into_owned();
            let parent_text = parent.to_string_lossy().into_owned();
            let shell_physical = shell_root.join(&srel);
            let shell_parent = shell_physical.parent().expect("parent").to_path_buf();
            let shell_parent_text = shell_parent.to_string_lossy().into_owned();
            let preamble =
                quarantine_preamble(&shell_root, &ssnapshot.0, &ssnapshot.1, &soverlays, &senv);
            let snippet = format!(
                "{preamble}_overlay_quarantine_rollback_link \"$2\"; code=$?; \
                 id=NONE; [ -n \"$OVERLAY_ADOPTION_PARKED\" ] && id=$(_overlay_replacement_identity \"$OVERLAY_ADOPTION_PARKED\" 2>/dev/null || echo NONE); \
                 match=no; [ -n \"$id\" ] && [ \"$id\" = \"$OVERLAY_ADOPTION_EXPECTED\" ] && match=yes; \
                 prefix=no; case \"$OVERLAY_ADOPTION_STAGE\" in {shell_parent_text}/.{base}.dot-overlay-adopt.*) prefix=yes;; esac; \
                 mode=$(stat -c '%a' \"$OVERLAY_ADOPTION_STAGE\" 2>/dev/null || stat -f '%Lp' \"$OVERLAY_ADOPTION_STAGE\" 2>/dev/null || echo NONE); \
                 gone=no; [ ! -e \"$OVERLAY_ADOPTION_PHYSICAL\" ] && [ ! -L \"$OVERLAY_ADOPTION_PHYSICAL\" ] && gone=yes; \
                 plink=$(readlink \"$OVERLAY_ADOPTION_PARKED\" 2>/dev/null || echo NONE); \
                 printf 'rc=%s\\nmatch=%s\\nprefix=%s\\nmode=%s\\ngone=%s\\nplink=%s\\n' \"$code\" \"$match\" \"$prefix\" \"$mode\" \"$gone\" \"$plink\"\n"
            );
            let (code, out, serr) = shell_run(home, &[srel.as_ref()], &snippet);
            assert_eq!(code, 0, "harness exit for {label}");
            assert!(serr.is_empty(), "quarantine stderr for {label}: {serr:?}");
            let shell = String::from_utf8(out).expect("quarantine dump");
            // Absolute link bytes differ per side; abstract the side
            // root so the comparison still pins kind and shape.
            let shell = shell.replacen(&shell_text, "@ROOT@", 10);
            let rust = match repos_overlays::quarantine_rollback_link(&rrel, &inputs) {
                repos_overlays::QuarantineOutcome::Adopt(adoption) => {
                    let id = repos_overlays::replacement_identity(&rust_root, &adoption.parked)
                        .unwrap_or_default();
                    let matched = if !id.is_empty() && id == adoption.expected {
                        "yes"
                    } else {
                        "no"
                    };
                    let prefixed = adoption
                        .stage
                        .to_string_lossy()
                        .starts_with(&format!("{parent_text}/.{base}.dot-overlay-adopt."));
                    let mode = dot::temp::file_mode(&adoption.stage)
                        .map(|mode| format!("{mode:o}"))
                        .unwrap_or_else(|_| "NONE".to_string());
                    let gone = std::fs::symlink_metadata(&adoption.physical).is_err();
                    let plink = std::fs::read_link(&adoption.parked)
                        .map(|target| target.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "NONE".to_string());
                    let rust = format!(
                        "rc=0\nmatch={matched}\nprefix={}\nmode={mode}\ngone={}\nplink={plink}\n",
                        if prefixed { "yes" } else { "no" },
                        if gone { "yes" } else { "no" },
                    );
                    rust.replacen(&rust_text, "@ROOT@", 10)
                }
                other => panic!("quarantine {label} refused: rc={}", other.code()),
            };
            assert_eq!(rust, shell, "quarantine aftermath for {label}");
        } else {
            let preamble =
                quarantine_preamble(&shell_root, &ssnapshot.0, &ssnapshot.1, &soverlays, &senv);
            let shell_physical = shell_root.join(&srel);
            let shell_parent = shell_physical.parent().expect("parent").to_path_buf();
            let snippet = format!("{preamble}{}", refused_probe());
            let (code, out, serr) = shell_run(
                home,
                &[
                    srel.as_ref(),
                    shell_physical.as_os_str(),
                    shell_parent.as_os_str(),
                ],
                &snippet,
            );
            assert_eq!(code, 0, "harness exit for {label}");
            assert!(serr.is_empty(), "quarantine stderr for {label}: {serr:?}");
            let shell = String::from_utf8(out).expect("quarantine dump");
            let shell = shell.replacen(&shell_text, "@ROOT@", 10);
            let rust_code = repos_overlays::quarantine_rollback_link(&rrel, &inputs).code();
            let rust = refused_rust(rust_code, &physical, &parent);
            let rust = rust.replacen(&rust_text, "@ROOT@", 10);
            assert_eq!(rust, shell, "quarantine refusal for {label}");
        }
    }
}
