//! Differential parity tests for the doctor orchestrator
//! (`lib/dot/doctor.sh`) against the live shell: the load boundary,
//! the runtime and engine-source checks, one extension run, and the
//! `_dot_doctor` skeleton.
//!
//! The live shell functions are the oracle: every row runs the real
//! `_dot_doctor_load` / `_dr_check_runtime` /
//! `_dr_check_engine_source` / `_dot_doctor_run_extension` /
//! `_dot_doctor` in a child bash and byte-compares its stdout (plus
//! exit status where the shell returns one) against the Rust port.
//! Children run under pipes with a gum-free `PATH` (`/usr/bin:/bin`
//! plus per-row fixture bins), so `_DR_*` colors are empty and the
//! `dot_ui_*` helpers take their plain branches — the exact
//! projection [`dot::doctor_orchestrator::Recorder::render`]
//! reproduces.
//!
//! Seams owned by other lanes stay live on the shell side and arrive
//! as adapters on the Rust side: kernel checks, extension discovery,
//! and the worker spawn are stubbed or hooked identically per row,
//! and the result-file dispatch uses a test-only TSV parser (the
//! shell side always runs the live `_dot_doctor_render_records`,
//! so any adapter drift fails the test — resolve drift against
//! `doctor.sh:121-133`, never by weakening the oracle).
//!
//! Portability: tests never call bare GNU `stat -c` (BSD macOS
//! rejects it). Shell snippets use the `stat -c ... || stat -f ...`
//! idiom; the Rust side reads modes in-process via
//! `PermissionsExt`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use dot::doctor_orchestrator::{
    EngineSnapshot, ExtensionRunner, Kernel, Loader, Recorder, RuntimeSnapshot, SECTION_FILES,
    check_engine_source, check_runtime, create_result_file, make_temp_dir, physical_dir,
    result_paths, run_doctor, run_extension, sections_present, split_spec, summary_color,
    summary_line,
};
use dot::test_support::TempDir;

/// Serializes tests that depend on ambient process environment
/// (`TMPDIR`/`HOME` reads inside `run_extension`, and the
/// set/restore cycle of the `from_env` resolution test). Child
/// processes are unaffected (they get explicit environments); only
/// in-process reads need the lock.
static ENV_GUARD: Mutex<()> = Mutex::new(());

/// Lock the ambient-environment guard (see [`ENV_GUARD`]).
fn lock_env() -> MutexGuard<'static, ()> {
    // Poisoning is ignored: `SavedEnv` restores on unwind, so a
    // prior failure leaves the environment intact and later tests
    // still report their own (not cascading) results.
    ENV_GUARD
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Saved process environment for scoped mutation.
struct SavedEnv {
    saved: Vec<(String, Option<String>)>,
}

impl SavedEnv {
    /// Set each `(key, Some(value))` and remove each
    /// `(key, None)`, remembering the previous state.
    fn apply(vars: &[(&str, Option<&str>)]) -> Self {
        let mut saved = Vec::new();
        for (key, value) in vars {
            saved.push((key.to_string(), std::env::var(key).ok()));
            match value {
                Some(text) => unsafe { std::env::set_var(key, text) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        SavedEnv { saved }
    }
}

impl Drop for SavedEnv {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..) {
            match value {
                Some(text) => unsafe { std::env::set_var(&key, text) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
    }
}

/// Shell libraries for the orchestrator rows.
const SOURCES: &str = concat!(
    ". \"$1/lib/dot/resources.sh\"\n",
    ". \"$1/lib/dot/temp.sh\"\n",
    ". \"$1/lib/dot/overlay-context.sh\"\n",
    ". \"$1/lib/dot/extension-worker-launch.sh\"\n",
    ". \"$1/lib/dot/doctor/runtime.sh\"\n",
    ". \"$1/lib/dot/doctor/paths.sh\"\n",
    ". \"$1/lib/dot/doctor.sh\"\n",
    ". \"$1/lib/dot/public/ui.sh\"\n",
);

/// `PATH` without gum (gum styles even under pipes, which would
/// leave the plain-branch projection). `/usr/bin` carries bash,
/// git, tr, mktemp, chmod, and stat.
const GUM_FREE_PATH: &str = "/usr/bin:/bin";

/// Run one shell snippet with the orchestrator libraries sourced.
/// `extra` adds (or overrides) child environment entries;
/// `bin_first` prepends one fixture bin directory to `PATH` (fake
/// tools win over `/usr/bin`). Returns (exit, stdout, stderr).
fn shell_run(
    home: &Path,
    tmpdir: &Path,
    bin_first: Option<&Path>,
    source_root: &Path,
    extra: &[(&str, &str)],
    snippet: &str,
) -> (i32, Vec<u8>, Vec<u8>) {
    let repo = env!("CARGO_MANIFEST_DIR");
    let mut path = String::new();
    if let Some(bin) = bin_first {
        path.push_str(&bin.to_string_lossy());
        path.push(':');
    }
    path.push_str(GUM_FREE_PATH);
    let mut cmd = Command::new(dot::test_support::bash());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(format!("{SOURCES}{snippet}"));
    cmd.arg("dot-test-sh").arg(repo);
    cmd.env_clear()
        .env("PATH", &path)
        .env("TMPDIR", tmpdir)
        .env("HOME", home)
        .env("DOT_SOURCE_ROOT", source_root)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra {
        cmd.env(key, value);
    }
    // Forked tools with locale-sensitive diagnostics must speak the
    // same ambient locale on both engines; the fixtures are ASCII
    // so parsing stays deterministic.
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

/// Ambient locale variables passed to shell children.
fn locale_passthrough() -> Vec<(String, String)> {
    ["LANG", "LC_ALL", "LC_MESSAGES", "LC_CTYPE", "LANGUAGE"]
        .into_iter()
        .filter_map(|key| {
            std::env::var_os(key)
                .map(|value| (key.to_string(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

/// Single-quote a word for snippet embedding.
fn sq(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The oracle bash's own version probe.
fn oracle_bash() -> (Vec<u8>, u64) {
    let output = Command::new(dot::test_support::bash())
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg("printf '%s\\n%s' \"$BASH_VERSION\" \"${BASH_VERSINFO[0]}\"")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("probe bash version");
    assert!(output.status.success(), "bash version probe");
    let mut parts = output.stdout.splitn(2, |byte| *byte == b'\n');
    let version = parts.next().unwrap_or_default().to_vec();
    let major = parts.next().unwrap_or_default();
    let major = std::str::from_utf8(major)
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0);
    assert!(!version.is_empty() && major >= 4, "oracle needs bash 4+");
    (version, major)
}

/// Run git for fixtures with a pinned identity.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", dir.display());
}

/// Shell `$(...)` semantics for one command line: success plus
/// trailing newlines stripped; anything else is unavailable.
fn cmd_line(cmd: &mut Command) -> Option<Vec<u8>> {
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut bytes = output.stdout;
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.is_empty() { None } else { Some(bytes) }
}

/// Canonicalized bytes for `dir` (both engines resolve real
/// directories identically), or an empty vec when unresolvable —
/// like the shell's `|| true` assignments.
fn canonical_or_empty(dir: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt as _;
    std::fs::canonicalize(dir)
        .map(|path| path.as_os_str().as_bytes().to_vec())
        .unwrap_or_default()
}

/// Build the [`EngineSnapshot`] the shell derives from `source`
/// with managed/development roots that do not exist (both resolve
/// to `None`, like the shell's `[[ ! -d ... ]]` skip). Rows using
/// this leave `SHDEPS_*` unset and run under a fresh `HOME`, so the
/// shell agrees on every field. Used by rows that bypass
/// `from_env`.
fn engine_literal(source: &Path) -> EngineSnapshot {
    let source_raw = source.to_string_lossy().into_owned().into_bytes();
    EngineSnapshot {
        source_raw: source_raw.clone(),
        managed_raw: b"/nonexistent-managed/cgraf78/dot".to_vec(),
        development_raw: b"/nonexistent-development/dot".to_vec(),
        source_real: physical_dir(&source_raw).unwrap_or_else(|| source_raw.clone()),
        managed_real: None,
        development_real: None,
        ignore_dev_checkout: false,
    }
}

/// Build the [`RuntimeSnapshot`] the shell derives for `source`:
/// live oracle bash version, sanitized rev-parse checkout root,
/// plain `git --version` line, and the given config default.
fn runtime_snapshot(source: &Path, config: &[u8], path_env: &str) -> RuntimeSnapshot {
    let (bash_version, bash_major) = oracle_bash();
    let source_raw = source.to_string_lossy().into_owned().into_bytes();
    let checkout_root = {
        use std::os::unix::ffi::OsStrExt as _;
        let mut cmd = dot::temp::sanitized_git(source, &["rev-parse", "--show-toplevel"]);
        // Mirror `VAR=... cmd` prefix additions without `envs`:
        // none needed — `sanitized_git` already binds everything.
        cmd.env("PATH", path_env);
        match cmd_line(&mut cmd) {
            Some(raw) => {
                let path = Path::new(std::ffi::OsStr::from_bytes(&raw)).to_path_buf();
                physical_dir(path.as_os_str().as_bytes())
            }
            None => None,
        }
    };
    let mut git_cmd = Command::new("git");
    git_cmd.arg("--version").env("PATH", path_env);
    RuntimeSnapshot {
        bash_version,
        bash_major,
        checkout_root,
        source_raw: source_raw.clone(),
        source_root: canonical_or_empty(source),
        git_version: cmd_line(&mut git_cmd),
        config_version: config.to_vec(),
    }
}

#[test]
fn load_publishes_sections_once() {
    // TempDir honors ambient TMPDIR, which the tmpdir-fail
    // row mutates: serialize with the guard.
    let _guard = lock_env();
    let dir = TempDir::new("doctor-load").expect("fixture dir");
    let home = dir.path().join("home");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&home).expect("home dir");
    std::fs::create_dir_all(&tmp).expect("tmp dir");
    let snippet = concat!(
        "printf 'loaded=%s\\n' \"$_DOT_DOCTOR_LOADED\"; ",
        "printf 'dir=%s\\n' \"$_DOT_DOCTOR_DIR\"; ",
        "_dot_doctor_load; ",
        "printf 'loaded=%s\\n' \"$_DOT_DOCTOR_LOADED\"; ",
        "_dot_doctor_load; ",
        "printf 'loaded=%s\\n' \"$_DOT_DOCTOR_LOADED\"; ",
        "for f in runtime.sh paths.sh repos.sh lock.sh provider.sh overlays.sh merges.sh; do ",
        "if [[ -f $_DOT_DOCTOR_DIR/$f ]]; then printf 'file=%s:yes\\n' \"$f\"; ",
        "else printf 'file=%s:no\\n' \"$f\"; fi; done; ",
    );
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (code, out, err) = shell_run(&home, &tmp, None, repo, &[], snippet);
    assert_eq!(code, 0, "harness exit");
    assert!(err.is_empty(), "shell stderr: {err:?}");
    let shell = String::from_utf8(out).expect("shell dump");
    let doctor_dir = repo.join("lib/dot/doctor");
    let mut loader = Loader::new();
    let mut rust = format!("loaded={}\n", if loader.is_loaded() { 1 } else { 0 });
    rust.push_str(&format!("dir={}\n", doctor_dir.display()));
    let first = loader.load(&doctor_dir);
    assert!(first.is_some(), "first load publishes");
    rust.push_str(&format!(
        "loaded={}\n",
        if loader.is_loaded() { 1 } else { 0 }
    ));
    assert!(loader.load(&doctor_dir).is_none(), "second load is a no-op");
    rust.push_str(&format!(
        "loaded={}\n",
        if loader.is_loaded() { 1 } else { 0 }
    ));
    for name in SECTION_FILES {
        let present = doctor_dir.join(name).is_file();
        rust.push_str(&format!(
            "file={name}:{}\n",
            if present { "yes" } else { "no" }
        ));
    }
    assert!(sections_present(&doctor_dir), "section files exist");
    assert_eq!(rust, shell, "load boundary");
}

#[test]
fn physical_dir_matches_cd_p() {
    // TempDir honors ambient TMPDIR, which the tmpdir-fail
    // row mutates: serialize with the guard.
    let _guard = lock_env();
    let dir = TempDir::new("doctor-physical").expect("fixture dir");
    let real = dir.path().join("real");
    let missing = dir.path().join("missing");
    let file = dir.path().join("file.txt");
    let dangling = dir.path().join("dangling");
    std::fs::create_dir_all(&real).expect("real dir");
    std::fs::write(&file, b"x").expect("file");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, dir.path().join("link")).expect("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&missing, &dangling).expect("dangling");
    let home = dir.path().join("home");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&home).expect("home dir");
    std::fs::create_dir_all(&tmp).expect("tmp dir");
    let cases: Vec<Vec<u8>> = vec![
        real.to_string_lossy().into_owned().into_bytes(),
        dir.path()
            .join("link")
            .to_string_lossy()
            .into_owned()
            .into_bytes(),
        missing.to_string_lossy().into_owned().into_bytes(),
        file.to_string_lossy().into_owned().into_bytes(),
        dangling.to_string_lossy().into_owned().into_bytes(),
        b"".to_vec(),
        format!("{}/", real.display()).into_bytes(),
        format!("{}/sub/..", real.display()).into_bytes(),
    ];
    let mut snippet = String::new();
    for (index, case) in cases.iter().enumerate() {
        let escaped = String::from_utf8_lossy(case).replace('\'', "'\\''");
        snippet.push_str(&format!(
            "P='{escaped}'; if R=$(cd -P -- \"$P\" 2>/dev/null && pwd -P); then printf 'row{index}=%s\\n' \"$R\"; else printf 'row{index}=NONE\\n'; fi; ",
        ));
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (code, out, err) = shell_run(&home, &tmp, None, repo, &[], &snippet);
    assert_eq!(code, 0, "harness exit");
    assert!(err.is_empty(), "shell stderr: {err:?}");
    let shell = String::from_utf8(out).expect("shell dump");

    let mut rust = String::new();
    for (index, case) in cases.iter().enumerate() {
        match physical_dir(case) {
            Some(resolved) => rust.push_str(&format!(
                "row{index}={}\n",
                String::from_utf8_lossy(&resolved)
            )),
            None => rust.push_str(&format!("row{index}=NONE\n")),
        }
    }
    assert_eq!(rust, shell, "physical dir rows");
}

/// Run one `_dr_check_runtime` row: the live shell against
/// (`home`, `source`) plus `extra` env, versus `check_runtime`
/// over the same derived snapshots. `config` is the
/// `DOT_CONFIG_VERSION` assignment prefix (`None` leaves it unset
/// for the `:-1` default, `Some("")` tests the empty spelling).
#[allow(clippy::too_many_arguments)]
fn runtime_row(
    tag: &str,
    home: &Path,
    tmp: &Path,
    source: &Path,
    bin_first: Option<&Path>,
    extra: &[(&str, &str)],
    config: Option<&str>,
    engine: &EngineSnapshot,
) {
    let snippet = match config {
        Some(value) => format!("DOT_CONFIG_VERSION={} _dr_check_runtime", sq(value)),
        None => "_dr_check_runtime".to_string(),
    };
    let (code, out, err) = shell_run(home, tmp, bin_first, source, extra, &snippet);
    assert_eq!(code, 0, "harness exit for {tag}");
    assert!(err.is_empty(), "shell stderr for {tag}: {err:?}");

    let mut path_env = String::new();
    if let Some(bin) = bin_first {
        path_env.push_str(&bin.to_string_lossy());
        path_env.push(':');
    }
    path_env.push_str(GUM_FREE_PATH);
    // `${DOT_CONFIG_VERSION:-1}` defaults unset AND empty alike.
    let config = match config {
        Some(value) if !value.is_empty() => value,
        _ => "1",
    };
    let snapshot = runtime_snapshot(source, config.as_bytes(), &path_env);
    let home_bytes = home.to_string_lossy().into_owned().into_bytes();
    let mut rec = Recorder::new();
    check_runtime(&mut rec, &snapshot, engine, &home_bytes);
    assert_eq!(rec.render(), out, "runtime bytes for {tag}");
}

#[test]
fn runtime_check_agrees() {
    // TempDir honors ambient TMPDIR, which the tmpdir-fail
    // row mutates: serialize with the guard.
    let _guard = lock_env();
    // Healthy git checkout, outside managed locations.
    {
        let dir = TempDir::new("doctor-rt-healthy").expect("fixture dir");
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        let source = dir.path().join("source");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::create_dir_all(&source).expect("source");
        git(&source, &["init", "-q"]);
        let engine = engine_literal(&source);
        runtime_row("healthy", &home, &tmp, &source, None, &[], None, &engine);
    }
    // Checkout under HOME abbreviates to `~/...`; development root
    // under HOME classifies the engine with a tilde display.
    {
        let dir = TempDir::new("doctor-rt-tilde").expect("fixture dir");
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        let source = home.join("git/dot");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::create_dir_all(&source).expect("source");
        git(&source, &["init", "-q"]);
        let home_text = home.to_string_lossy().into_owned();
        let source_raw = source.to_string_lossy().into_owned().into_bytes();
        let engine = EngineSnapshot {
            source_raw: source_raw.clone(),
            managed_raw: format!("{home_text}/.local/share/cgraf78/dot").into_bytes(),
            development_raw: format!("{home_text}/git/dot").into_bytes(),
            source_real: physical_dir(&source_raw).unwrap_or_else(|| source_raw.clone()),
            managed_real: None,
            development_real: physical_dir(&source_raw),
            ignore_dev_checkout: false,
        };
        runtime_row(
            "tilde-dev",
            &home,
            &tmp,
            &source,
            None,
            &[],
            Some("9"),
            &engine,
        );
    }
    // Managed checkout via SHDEPS_INSTALL_DIR, empty config default.
    {
        let dir = TempDir::new("doctor-rt-managed").expect("fixture dir");
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        let install = dir.path().join("managed");
        let source = install.join("cgraf78/dot");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::create_dir_all(&source).expect("source");
        git(&source, &["init", "-q"]);
        let install_text = install.to_string_lossy().into_owned();
        let source_raw = source.to_string_lossy().into_owned().into_bytes();
        let source_real = physical_dir(&source_raw).unwrap_or_else(|| source_raw.clone());
        let engine = EngineSnapshot {
            source_raw: source_raw.clone(),
            managed_raw: format!("{install_text}/cgraf78/dot").into_bytes(),
            development_raw: b"/nonexistent-development/dot".to_vec(),
            source_real: source_real.clone(),
            managed_real: Some(source_real),
            development_real: None,
            ignore_dev_checkout: false,
        };
        let extra = [("SHDEPS_INSTALL_DIR", install_text.as_str())];
        runtime_row(
            "managed",
            &home,
            &tmp,
            &source,
            None,
            &extra,
            Some(""),
            &engine,
        );
    }
    // Plain directory: checkout unavailable; bypass plus outside.
    {
        let dir = TempDir::new("doctor-rt-plain").expect("fixture dir");
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        let source = dir.path().join("plain");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::create_dir_all(&source).expect("source");
        let mut engine = engine_literal(&source);
        engine.ignore_dev_checkout = true;
        let extra = [("DOT_IGNORE_DEV_CHECKOUT", "1")];
        runtime_row(
            "plain-bypass",
            &home,
            &tmp,
            &source,
            None,
            &extra,
            None,
            &engine,
        );
    }
    // Dead git: checkout and Git runtime both unavailable.
    {
        let dir = TempDir::new("doctor-rt-nogit").expect("fixture dir");
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        let source = dir.path().join("source");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::create_dir_all(&bin).expect("bin");
        git(&source, &["init", "-q"]);
        std::fs::write(bin.join("git"), "#!/bin/sh\nexit 1\n").expect("fake git");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(bin.join("git"), std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake git");
        }
        let engine = engine_literal(&source);
        runtime_row(
            "dead-git",
            &home,
            &tmp,
            &source,
            Some(&bin),
            &[],
            None,
            &engine,
        );
    }
}

/// Run one `_dr_check_engine_source` row against a literal Rust
/// snapshot; the shell derives the same snapshot from `extra` env
/// under (`home`, `source`).
fn engine_row(
    tag: &str,
    home: &Path,
    tmp: &Path,
    source: &Path,
    extra: &[(&str, &str)],
    engine: &EngineSnapshot,
) {
    let (code, out, err) = shell_run(home, tmp, None, source, extra, "_dr_check_engine_source");
    assert_eq!(code, 0, "harness exit for {tag}");
    assert!(err.is_empty(), "shell stderr for {tag}: {err:?}");
    let home_bytes = home.to_string_lossy().into_owned().into_bytes();
    let mut rec = Recorder::new();
    check_engine_source(&mut rec, engine, &home_bytes);
    assert_eq!(rec.render(), out, "engine bytes for {tag}");
}

#[test]
fn engine_source_check_agrees() {
    // TempDir honors ambient TMPDIR, which the tmpdir-fail
    // row mutates: serialize with the guard.
    let _guard = lock_env();
    // Outside with a non-1 bypass value: no bypass notice.
    {
        let dir = TempDir::new("doctor-eng-outside").expect("fixture dir");
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        let source = dir.path().join("elsewhere");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::create_dir_all(&source).expect("source");
        let engine = engine_literal(&source);
        engine_row(
            "outside",
            &home,
            &tmp,
            &source,
            &[("DOT_IGNORE_DEV_CHECKOUT", "2")],
            &engine,
        );
    }
    // Managed root under HOME abbreviates; development wins when
    // both resolve to the source (symlinked dev dir).
    {
        let dir = TempDir::new("doctor-eng-managed").expect("fixture dir");
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        let install = home.join("m");
        let source = install.join("cgraf78/dot");
        let dev = dir.path().join("dev/dot");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::create_dir_all(&source).expect("source");
        std::fs::create_dir_all(dir.path().join("dev")).expect("dev parent");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, &dev).expect("dev link");
        let install_text = install.to_string_lossy().into_owned();
        let dev_parent = dir.path().join("dev").to_string_lossy().into_owned();
        let source_raw = source.to_string_lossy().into_owned().into_bytes();
        let managed_raw = format!("{install_text}/cgraf78/dot").into_bytes();
        let development_raw = format!("{dev_parent}/dot").into_bytes();
        let engine = EngineSnapshot {
            source_raw: source_raw.clone(),
            source_real: physical_dir(&source_raw).unwrap_or_else(|| source_raw.clone()),
            managed_real: physical_dir(&managed_raw),
            development_real: physical_dir(&development_raw),
            managed_raw,
            development_raw,
            ignore_dev_checkout: false,
        };
        let extra = [
            ("SHDEPS_INSTALL_DIR", install_text.as_str()),
            ("SHDEPS_GIT_DEV_DIR", dev_parent.as_str()),
        ];
        // The dev dir is a symlink to the source (unix): both
        // resolve identically and development wins. Elsewhere the
        // dev dir is absent and the same source classifies managed
        // with a `~/...` display.
        engine_row("dev-wins", &home, &tmp, &source, &extra, &engine);
        #[cfg(unix)]
        std::fs::remove_file(&dev).expect("remove dev link");
        let managed_raw = format!("{install_text}/cgraf78/dot").into_bytes();
        let engine = EngineSnapshot {
            source_raw: source_raw.clone(),
            source_real: physical_dir(&source_raw).unwrap_or_else(|| source_raw.clone()),
            managed_real: physical_dir(&managed_raw),
            development_real: None,
            managed_raw,
            development_raw: b"/nonexistent-development/dot".to_vec(),
            ignore_dev_checkout: false,
        };
        engine_row("managed-tilde", &home, &tmp, &source, &extra[..1], &engine);
    }
}

#[test]
fn from_env_resolution_agrees() {
    let _guard = lock_env();
    // `install`/`dev`/`ignore`: None unsets, Some("") tests the
    // `${var:-default}` empty spelling.
    struct EnvRow {
        tag: &'static str,
        install: Option<&'static str>,
        dev: Option<&'static str>,
        ignore: Option<&'static str>,
    }
    let rows = [
        EnvRow {
            tag: "defaults",
            install: None,
            dev: None,
            ignore: None,
        },
        EnvRow {
            tag: "custom-ignore",
            install: Some("INSTALL"),
            dev: Some("DEV"),
            ignore: Some("1"),
        },
        EnvRow {
            tag: "empty-means-default",
            install: Some(""),
            dev: Some(""),
            ignore: Some("0"),
        },
        EnvRow {
            tag: "install-is-file",
            install: Some("FILE"),
            dev: None,
            ignore: Some("2"),
        },
    ];
    for row in &rows {
        let tag = row.tag;
        let dir = TempDir::new("doctor-fromenv").expect("fixture dir");
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        let source = dir.path().join("source");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&tmp).expect("tmp");
        std::fs::create_dir_all(&source).expect("source");
        // Resolve INSTALL/DEV markers to fixture paths.
        let install_path = match row.install {
            Some("INSTALL") => {
                let path = dir.path().join("install");
                std::fs::create_dir_all(&path).expect("install");
                Some(path.to_string_lossy().into_owned())
            }
            Some("FILE") => {
                let path = dir.path().join("afile");
                std::fs::write(&path, b"x").expect("file");
                Some(path.to_string_lossy().into_owned())
            }
            Some("") => Some(String::new()),
            _ => None,
        };
        let dev_path = match row.dev {
            Some("DEV") => {
                let path = dir.path().join("devbase");
                std::fs::create_dir_all(&path).expect("devbase");
                Some(path.to_string_lossy().into_owned())
            }
            Some("") => Some(String::new()),
            _ => None,
        };
        let vars = vec![
            ("SHDEPS_INSTALL_DIR", install_path.as_deref()),
            ("SHDEPS_GIT_DEV_DIR", dev_path.as_deref()),
            ("DOT_IGNORE_DEV_CHECKOUT", row.ignore),
        ];
        let _saved = SavedEnv::apply(&vars);
        let mut extra: Vec<(&str, &str)> = Vec::new();
        if let Some(value) = &install_path {
            extra.push(("SHDEPS_INSTALL_DIR", value));
        }
        if let Some(value) = &dev_path {
            extra.push(("SHDEPS_GIT_DEV_DIR", value));
        }
        if let Some(value) = row.ignore {
            extra.push(("DOT_IGNORE_DEV_CHECKOUT", value));
        }
        let snippet = concat!(
            "managed=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}/cgraf78/dot; ",
            "development=${SHDEPS_GIT_DEV_DIR:-$HOME/git}/dot; ",
            "printf 'managed_raw=%s\\n' \"$managed\"; ",
            "printf 'development_raw=%s\\n' \"$development\"; ",
            "source_real=$(cd -P -- \"$DOT_SOURCE_ROOT\" 2>/dev/null && pwd -P) || source_real=$DOT_SOURCE_ROOT; ",
            "printf 'source_real=%s\\n' \"$source_real\"; ",
            "if [[ -d $managed ]] && R=$(cd -P -- \"$managed\" 2>/dev/null && pwd -P); then printf 'managed_real=%s\\n' \"$R\"; else printf 'managed_real=NONE\\n'; fi; ",
            "if [[ -d $development ]] && R=$(cd -P -- \"$development\" 2>/dev/null && pwd -P); then printf 'development_real=%s\\n' \"$R\"; else printf 'development_real=NONE\\n'; fi; ",
            "if [[ ${DOT_IGNORE_DEV_CHECKOUT:-0} == 1 ]]; then printf 'ignore=1\\n'; else printf 'ignore=0\\n'; fi; ",
        );
        let (code, out, err) = shell_run(&home, &tmp, None, &source, &extra, snippet);
        assert_eq!(code, 0, "harness exit for {tag}");
        assert!(err.is_empty(), "shell stderr for {tag}: {err:?}");
        let shell = String::from_utf8(out).expect("shell dump");

        let source_raw = source.to_string_lossy().into_owned().into_bytes();
        let home_bytes = home.to_string_lossy().into_owned().into_bytes();
        let snapshot = EngineSnapshot::from_env(&source_raw, &home_bytes);
        let mut rust = format!(
            "managed_raw={}\ndevelopment_raw={}\nsource_real={}\n",
            String::from_utf8_lossy(&snapshot.managed_raw),
            String::from_utf8_lossy(&snapshot.development_raw),
            String::from_utf8_lossy(&snapshot.source_real),
        );
        match &snapshot.managed_real {
            Some(real) => {
                rust.push_str(&format!("managed_real={}\n", String::from_utf8_lossy(real)))
            }
            None => rust.push_str("managed_real=NONE\n"),
        }
        match &snapshot.development_real {
            Some(real) => {
                rust.push_str(&format!(
                    "development_real={}\n",
                    String::from_utf8_lossy(real)
                ));
            }
            None => rust.push_str("development_real=NONE\n"),
        }
        rust.push_str(&format!(
            "ignore={}\n",
            if snapshot.ignore_dev_checkout { 1 } else { 0 }
        ));
        assert_eq!(rust, shell, "from_env for {tag}");
    }
}

#[test]
fn summary_and_split_helpers_agree() {
    // TempDir honors ambient TMPDIR, which the tmpdir-fail
    // row mutates: serialize with the guard.
    let _guard = lock_env();
    let dir = TempDir::new("doctor-helpers").expect("fixture dir");
    let home = dir.path().join("home");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&tmp).expect("tmp");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    let snippet = concat!(
        "printf '%d passed · %d warnings · %d failed\\n' 6 1 2; ",
        "for spec in '1 0' '0 3' '0 0' '2 5'; do set -- $spec; ",
        "if [[ $1 -gt 0 ]]; then echo red; ",
        "elif [[ $2 -gt 0 ]]; then echo yellow; else echo green; fi; done; ",
        "printf 'a-one\\t/fake/a.sh\\n\\nkey\\tscript\\twith\\ttabs\\nno-tab\\n' | ",
        "while IFS= read -r line || [[ -n $line ]]; do ",
        "if [[ -z $line ]]; then echo SKIP; continue; fi; ",
        "IFS=$'\\t' read -r key script <<<\"$line\"; ",
        "printf 'key=%s script=%s\\n' \"$key\" \"$script\"; done; ",
    );
    let (code, out, err) = shell_run(&home, &tmp, None, repo, &[], snippet);
    assert_eq!(code, 0, "harness exit");
    assert!(err.is_empty(), "shell stderr: {err:?}");
    let shell = String::from_utf8(out).expect("shell dump");

    let mut rust = format!("{}\n", summary_line(6, 1, 2));
    for (fail, warn) in [(1, 0), (0, 3), (0, 0), (2, 5)] {
        rust.push_str(summary_color(fail, warn).name());
        rust.push('\n');
    }
    for line in ["a-one\t/fake/a.sh", "", "key\tscript\twith\ttabs", "no-tab"] {
        match split_spec(line.as_bytes()) {
            None => rust.push_str("SKIP\n"),
            Some((key, script)) => rust.push_str(&format!(
                "key={} script={}\n",
                String::from_utf8_lossy(key),
                String::from_utf8_lossy(script),
            )),
        }
    }
    assert_eq!(rust, shell, "summary and split helpers");
}

#[test]
fn temp_and_result_file_modes_agree() {
    let _guard = lock_env();
    let dir = TempDir::new("doctor-tempmodes").expect("fixture dir");
    let home = dir.path().join("home");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&tmp).expect("tmp");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
    // Portable mode probe (BSD `stat -f` fallback); the mandate
    // bans only bare GNU `stat -c`.
    let snippet = concat!(
        "T=$(mktemp -d \"$TMPDIR\"/dot.XXXXXXXX); ",
        "R=$T/results; L=$T/output; : >\"$R\"; chmod 0600 \"$R\"; ",
        "mode() { stat -c '%a' \"$1\" 2>/dev/null || stat -f '%Lp' \"$1\" 2>/dev/null || echo NONE; }; ",
        "printf 'tmpmode=%s\\n' \"$(mode \"$T\")\"; ",
        "printf 'resmode=%s\\n' \"$(mode \"$R\")\"; ",
        "printf 'names=%s,%s\\n' \"${R##*/}\" \"${L##*/}\"; ",
        "rm -rf \"$T\"; ",
    );
    let (code, out, err) = shell_run(&home, &tmp, None, repo, &[], snippet);
    assert_eq!(code, 0, "harness exit");
    assert!(err.is_empty(), "shell stderr: {err:?}");
    let shell = String::from_utf8(out).expect("shell dump");

    let tmp_text = tmp.to_string_lossy().into_owned();
    let _saved = SavedEnv::apply(&[("TMPDIR", Some(&tmp_text))]);
    let temporary = make_temp_dir().expect("make temp dir");
    assert!(
        temporary.starts_with(&tmp),
        "temp dir under TMPDIR: {}",
        temporary.display()
    );
    let (result, log) = result_paths(&temporary);
    assert_eq!(result.file_name(), Some(std::ffi::OsStr::new("results")));
    assert_eq!(log.file_name(), Some(std::ffi::OsStr::new("output")));
    create_result_file(&result).expect("create result file");
    use std::os::unix::fs::PermissionsExt as _;
    let tmp_mode = std::fs::metadata(&temporary)
        .expect("stat temp")
        .permissions()
        .mode()
        & 0o7777;
    let res_mode = std::fs::metadata(&result)
        .expect("stat result")
        .permissions()
        .mode()
        & 0o7777;
    let rust = format!("tmpmode={tmp_mode:o}\nresmode={res_mode:o}\nnames=results,output\n");
    std::fs::remove_dir_all(&temporary).expect("remove temp");
    assert_eq!(rust, shell, "temp and result file modes");
    assert_eq!((tmp_mode, res_mode), (0o700, 0o600), "exact modes");
}

/// Test-only adapter for the `_dot_doctor_render_records` seam
/// (owned by another lane): parse the result file into [`Recorder`]
/// rows. The shell side always runs the live function, so any
/// adapter drift fails the test — resolve drift against
/// `doctor.sh:121-133`, never by weakening the oracle.
fn render_adapter(result: &Path, rec: &mut Recorder) {
    let bytes = std::fs::read(result).unwrap_or_default();
    let mut chunks: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    // `read ... || [[ -n $kind ]]`: a final unterminated line still
    // processes; a trailing newline leaves no extra row.
    if chunks.last() == Some(&b"".as_slice()) {
        chunks.pop();
    }
    for chunk in chunks {
        // `read` with three variables keeps the remainder (with
        // tabs) in the last one; missing fields read empty.
        let mut parts = chunk.splitn(3, |byte| *byte == b'\t');
        let kind = parts.next().unwrap_or(b"");
        let message = parts.next().unwrap_or(b"");
        let detail = parts.next().unwrap_or(b"");
        match kind {
            b"section" => rec.section(message),
            b"ok" => rec.ok(message, Some(detail)),
            b"warn" => rec.warn(message, Some(detail)),
            b"fail" => rec.fail(message, Some(detail)),
            b"skip" => rec.skip(message, Some(detail)),
            _ => rec.fail(b"doctor extension emitted an invalid result", Some(kind)),
        }
    }
}

/// One `_dot_doctor_run_extension` row: `rows`/`log` travel with
/// literal tabs and newlines (no escape layer on either side),
/// `rc` is the worker status, and `overlays` feeds
/// `ACTIVE_OVERLAYS` (`&["bogus"]` forces the context failure).
struct ExtRow {
    tag: &'static str,
    key: &'static str,
    rows: &'static str,
    log: &'static str,
    rc: i32,
    overlays: &'static [&'static str],
    tmpdir_file: bool,
}

fn ext_rows() -> Vec<ExtRow> {
    vec![
        ExtRow {
            tag: "quiet-ok",
            key: "mykey",
            rows: "ok\text health\tdetail here\nsection\tsec only\n",
            log: "",
            rc: 0,
            overlays: &[],
            tmpdir_file: false,
        },
        ExtRow {
            tag: "stray-log",
            key: "mykey",
            rows: "ok\text health\tdetail here\n",
            log: "stray-output\n",
            rc: 0,
            overlays: &[],
            tmpdir_file: false,
        },
        ExtRow {
            tag: "worker-fails",
            key: "mykey",
            rows: "warn\talmost\tcareful\n",
            log: "boom\nline2\n",
            rc: 3,
            overlays: &[],
            tmpdir_file: false,
        },
        ExtRow {
            tag: "invalid-kind",
            key: "mykey",
            rows: "bogus\tm\td\nskip\tskipped\twhy\n",
            log: "",
            rc: 0,
            overlays: &[],
            tmpdir_file: false,
        },
        ExtRow {
            tag: "empty-result",
            key: "mykey",
            rows: "",
            log: "",
            rc: 0,
            overlays: &[],
            tmpdir_file: false,
        },
        ExtRow {
            tag: "context-fails",
            key: "mykey",
            rows: "ok\text health\tdetail here\n",
            log: "",
            rc: 0,
            overlays: &["bogus"],
            tmpdir_file: false,
        },
        ExtRow {
            tag: "tmpdir-fails",
            key: "mykey",
            rows: "ok\text health\tdetail here\n",
            log: "",
            rc: 0,
            overlays: &[],
            tmpdir_file: true,
        },
    ]
}

#[test]
fn extension_run_agrees() {
    let _guard = lock_env();
    for row in ext_rows() {
        let dir = TempDir::new("doctor-ext").expect("fixture dir");
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&tmp).expect("tmp");
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mode_file = dir.path().join("mode");
        let tmp_file = dir.path().join("captured-tmp");
        let block_file = dir.path().join("block");
        if row.tmpdir_file {
            std::fs::write(&block_file, b"not a directory").expect("block file");
        }
        let mut overlays_snippet = String::from("ACTIVE_OVERLAYS=(");
        for overlay in row.overlays {
            overlays_snippet.push_str(&sq(overlay));
            overlays_snippet.push(' ');
        }
        overlays_snippet.push_str("); ");
        let snippet = format!(
            concat!(
                "_dot_extension_worker_exec() {{ ",
                "printf '%s' \"$STUB_ROWS\" >>\"$4\"; ",
                "printf '%s' \"$STUB_LOG\"; ",
                "{{ stat -c '%a' \"$4\" 2>/dev/null || stat -f '%Lp' \"$4\" 2>/dev/null || echo NONE; }} >\"$CAPTURE_MODE\"; ",
                "printf '%s' \"$3\" >\"$CAPTURE_TMP\"; ",
                "return \"$STUB_RC\"; }}; ",
                "{overlays} ",
                "_dot_doctor_run_extension {key} '/fake/script.sh'{redir}; printf 'rc=%s\\n' \"$?\"; ",
                "captured=$(cat \"$CAPTURE_TMP\" 2>/dev/null || true); ",
                "if [[ -n $captured && -e $captured ]]; then echo LEAKED; else echo CLEANED; fi; ",
            ),
            overlays = overlays_snippet,
            key = sq(row.key),
            // The allocator's own mktemp diagnostic (locale-dependent
            // tool text owned by the resources lane) is out of scope.
            redir = if row.tmpdir_file { " 2>/dev/null" } else { "" },
        );
        let tmp_env = tmp.to_string_lossy().into_owned();
        let child_tmp = if row.tmpdir_file {
            block_file.to_string_lossy().into_owned()
        } else {
            tmp_env.clone()
        };
        // The child TMPDIR must be a directory for the non-failure
        // rows and the blocking file for the failure row; pass it
        // explicitly rather than through `tmp`.
        let child_tmp_path = PathBuf::from(child_tmp);
        let rc_text = row.rc.to_string();
        let mode_text = mode_file.to_str().expect("mode path").to_string();
        let tmp_text = tmp_file.to_str().expect("tmp path").to_string();
        let extra = [
            ("STUB_ROWS", row.rows),
            ("STUB_LOG", row.log),
            ("STUB_RC", rc_text.as_str()),
            ("CAPTURE_MODE", mode_text.as_str()),
            ("CAPTURE_TMP", tmp_text.as_str()),
        ];
        let (code, out, err) = shell_run(&home, &child_tmp_path, None, &repo, &extra, &snippet);
        assert_eq!(code, 0, "harness exit for {}", row.tag);
        // The context failure prints the overlay-context lane's own
        // diagnostic to stderr as it unwinds. That text is not this
        // lane's output, so the row pins it exactly instead of
        // comparing it as behavior.
        let want_err: &[u8] = if row.tag == "context-fails" {
            b"dot: overlay context: invalid overlay record\n"
        } else {
            b""
        };
        assert_eq!(err, want_err, "shell stderr for {}", row.tag);

        // The Rust side runs the same driver with a hook worker
        // writing the same bytes; ambient TMPDIR is pinned to the
        // fixture (or the blocking file) so `make_temp_dir`
        // succeeds or fails exactly like the shell allocator.
        let rust_tmp = if row.tmpdir_file {
            block_file.to_string_lossy().into_owned()
        } else {
            tmp_env.clone()
        };
        let _saved = SavedEnv::apply(&[("TMPDIR", Some(rust_tmp.as_str()))]);
        let rows_bytes = row.rows.as_bytes().to_vec();
        let log_bytes = row.log.as_bytes().to_vec();
        let mode_path = mode_file.clone();
        let tmp_path = tmp_file.clone();
        let mut worker = |inv: &dot::doctor_orchestrator::WorkerInvocation<'_>| -> i32 {
            use std::io::Write as _;
            assert_eq!(
                inv.script,
                Path::new("/fake/script.sh"),
                "worker script arg for {}",
                row.tag
            );
            let mut result = std::fs::OpenOptions::new()
                .append(true)
                .open(inv.result)
                .expect("result open");
            result.write_all(&rows_bytes).expect("write rows");
            std::fs::write(inv.log, &log_bytes).expect("write log");
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(inv.result)
                .map(|meta| format!("{:o}", meta.permissions().mode() & 0o7777))
                .unwrap_or_else(|_| "NONE".to_string());
            std::fs::write(&mode_path, mode + "\n").expect("mode capture");
            std::fs::write(&tmp_path, inv.temporary.as_os_str().as_encoded_bytes())
                .expect("tmp capture");
            row.rc
        };
        let mut render = |result: &Path, rec: &mut Recorder| render_adapter(result, rec);
        let mut rec = Recorder::new();
        let overlays: Vec<Vec<u8>> = row
            .overlays
            .iter()
            .map(|entry| entry.as_bytes().to_vec())
            .collect();
        let rc = run_extension(
            &mut rec,
            row.key.as_bytes(),
            Path::new("/fake/script.sh"),
            &overlays,
            &mut worker,
            &mut render,
        );
        let mut rust_out = rec.render();
        rust_out.extend_from_slice(format!("rc={rc}\n").as_bytes());
        let captured = std::fs::read(&tmp_file).unwrap_or_default();
        let cleaned =
            captured.is_empty() || !Path::new(String::from_utf8_lossy(&captured).as_ref()).exists();
        rust_out.extend_from_slice(if cleaned { b"CLEANED\n" } else { b"LEAKED\n" });
        assert_eq!(rust_out, out, "extension bytes for {}", row.tag);
        assert_eq!(
            rc,
            if row.tmpdir_file || !row.overlays.is_empty() {
                1
            } else {
                row.rc
            },
            "extension rc for {}",
            row.tag
        );
        // The result file mode travels out-of-band (the file is
        // removed before either side returns).
        if !row.tmpdir_file && row.overlays.is_empty() {
            let shell_mode = std::fs::read(&mode_file).expect("shell mode capture");
            let rust_mode = std::fs::read(&mode_file).expect("rust mode capture");
            assert_eq!(rust_mode, shell_mode, "result mode for {}", row.tag);
            assert_eq!(rust_mode, b"600\n", "result mode is 0600 for {}", row.tag);
        }
    }
}

/// Kernel stubs: the five out-of-scope checks in `_dot_doctor`
/// order, with identical record effects on both engines.
fn dirty_kernels_shell() -> &'static str {
    concat!(
        "_dr_check_base_repo() { _dr_ok 'base repo stub'; }; ",
        "_dr_check_update_lock() { _dr_warn 'lock stub' 'w detail'; }; ",
        "_dr_check_provider() { :; }; ",
        "_dr_check_overlays() { _dr_fail 'overlay stub'; }; ",
        "_dr_check_merges() { _dr_skip 'merge stub' 's detail'; }; ",
    )
}

/// All-clean kernels (no fails, no warns): the exit-0 row.
fn clean_kernels_shell() -> &'static str {
    concat!(
        "_dr_check_base_repo() { _dr_ok 'base repo stub'; }; ",
        "_dr_check_update_lock() { _dr_ok 'lock stub'; }; ",
        "_dr_check_provider() { _dr_skip 'provider stub'; }; ",
        "_dr_check_overlays() { _dr_ok 'overlay stub'; }; ",
        "_dr_check_merges() { _dr_ok 'merge stub'; }; ",
    )
}

/// One `_dot_doctor` skeleton row. Kernels, discovery, and the
/// runner are the out-of-scope seams, stubbed identically per row;
/// the live oracle is the load/title/order/loop/summary/exit
/// skeleton between them.
struct SkeletonRow {
    tag: &'static str,
    clean_kernels: bool,
    specs: &'static str,
    specs_fail: bool,
}

fn skeleton_rows() -> Vec<SkeletonRow> {
    vec![
        SkeletonRow {
            tag: "mixed-dirty",
            clean_kernels: false,
            specs: "a-one\t/fake/a.sh\n\nbad-wolf\t/fake/b.sh\nplainkey\n",
            specs_fail: false,
        },
        SkeletonRow {
            tag: "discovery-fails",
            clean_kernels: false,
            specs: "",
            specs_fail: true,
        },
        SkeletonRow {
            tag: "clean-yellow",
            clean_kernels: true,
            specs: "solo\t/fake/solo.sh\n",
            specs_fail: false,
        },
    ]
}

#[test]
fn doctor_skeleton_agrees() {
    // TempDir honors ambient TMPDIR, which the tmpdir-fail
    // row mutates: serialize with the guard.
    let _guard = lock_env();
    for row in skeleton_rows() {
        let dir = TempDir::new("doctor-skeleton").expect("fixture dir");
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(&tmp).expect("tmp");
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let kernels = if row.clean_kernels {
            clean_kernels_shell()
        } else {
            dirty_kernels_shell()
        };
        let snippet = format!(
            concat!(
                "_dot_doctor_load; ",
                "{kernels} ",
                "_dot_doctor_extension_specs() {{ ",
                "if [[ $SPECS_FAIL == 1 ]]; then return 1; else printf '%s' \"$SPECS\"; fi; }}; ",
                "_dot_doctor_run_extension() {{ ",
                "if [[ $1 == bad* ]]; then _dr_fail \"$1 broke\"; return 2; ",
                "else _dr_ok \"$1 ran\" \"$2\"; return 0; fi; }}; ",
                "_dot_doctor; printf 'exit=%s\\n' \"$?\"; ",
            ),
            kernels = kernels,
        );
        let extra = [
            ("SPECS", row.specs),
            ("SPECS_FAIL", if row.specs_fail { "1" } else { "0" }),
        ];
        let (code, out, err) = shell_run(&home, &tmp, None, &repo, &extra, &snippet);
        assert_eq!(code, 0, "harness exit for {}", row.tag);
        assert!(err.is_empty(), "shell stderr for {}: {err:?}", row.tag);

        let snapshot = runtime_snapshot(&repo, b"1", GUM_FREE_PATH);
        let engine = engine_literal(&repo);
        let home_bytes = home.to_string_lossy().into_owned().into_bytes();
        let mut rec = Recorder::new();
        let mut kernels: Vec<Kernel> = if row.clean_kernels {
            vec![
                Box::new(|rec: &mut Recorder| rec.ok(b"base repo stub", None)),
                Box::new(|rec: &mut Recorder| rec.ok(b"lock stub", None)),
                Box::new(|rec: &mut Recorder| rec.skip(b"provider stub", None)),
                Box::new(|rec: &mut Recorder| rec.ok(b"overlay stub", None)),
                Box::new(|rec: &mut Recorder| rec.ok(b"merge stub", None)),
            ]
        } else {
            vec![
                Box::new(|rec: &mut Recorder| rec.ok(b"base repo stub", None)),
                Box::new(|rec: &mut Recorder| rec.warn(b"lock stub", Some(b"w detail"))),
                Box::new(|_: &mut Recorder| ()),
                Box::new(|rec: &mut Recorder| rec.fail(b"overlay stub", None)),
                Box::new(|rec: &mut Recorder| rec.skip(b"merge stub", Some(b"s detail"))),
            ]
        };
        let discovery: Result<Vec<Vec<u8>>, ()> = if row.specs_fail {
            Err(())
        } else {
            // `specs=$(...)` strips trailing newlines; blank lines
            // skip in the loop via `split_spec`.
            Ok(row
                .specs
                .split('\n')
                .map(|line| line.as_bytes().to_vec())
                .collect())
        };
        let mut runner: ExtensionRunner = Box::new(|rec, key, script| {
            if key.starts_with(b"bad") {
                let mut message = key.to_vec();
                message.extend_from_slice(b" broke");
                rec.fail(&message, None);
                2
            } else {
                let mut message = key.to_vec();
                message.extend_from_slice(b" ran");
                rec.ok(&message, Some(script));
                0
            }
        });
        let mut rust_out = Vec::new();
        let ok = run_doctor(
            &mut rust_out,
            &mut rec,
            &snapshot,
            &engine,
            &home_bytes,
            &mut kernels,
            &discovery,
            &mut runner,
        );
        rust_out.extend_from_slice(format!("exit={}\n", if ok { 0 } else { 1 }).as_bytes());
        assert_eq!(rust_out, out, "skeleton bytes for {}", row.tag);
    }
}
