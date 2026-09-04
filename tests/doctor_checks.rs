//! Differential parity tests for the doctor check family
//! (`lib/dot/doctor/{lock,merges,overlays,provider,repos}.sh`)
//! against the live shell functions as oracle.
//!
//! Every check test runs the real shell function on a scratch
//! fixture, runs the Rust port on the same fixture, and
//! byte-compares the rendered records against the shell stdout
//! (piped, so the shell color variables are empty and the bytes
//! are deterministic). Status-only helpers compare their
//! `rc=`/`selected=` witness lines the same way.
//!
//! Heavy trust-policy helpers stay stubbed on the shell side with
//! the same data the Rust side receives as inputs (deactivation
//! probe, local-source validation, shdeps selection, lifecycle
//! load); filesystem and `git` probes the check itself performs
//! run for real on both sides. The `wc`-failure stub for the
//! merges invalid-inventory branch documents a defensive arm the
//! live pipeline cannot reach (a bad inventory still exits 0
//! through `sort`).
//!
//! Portability: no bare GNU `stat -c` anywhere (BSD rejects it);
//! fixture modes use Rust `PermissionsExt` in-process, and lock
//! aging uses POSIX `touch -t`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::doctor_checks::{
    BaseRepoInputs, LifecycleInputs, MergeInputs, OverlayInputs, ProviderInputs, ProviderInstaller,
    Record, check_base_repo, check_merges, check_overlays, check_profile_lifecycle, check_provider,
    check_update_lock, completed_identity_matches_home, is_client_checkout, render, shdeps_binary,
};
use dot::test_support::TempDir;

/// Oracle interpreter (see `dot::test_support::bash`): absolute
/// bash 4+ resolved from the parent PATH.
fn bash_bin() -> &'static Path {
    dot::test_support::bash()
}

/// Crate root, passed as `$1` so snippets source `lib/` in place.
fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Run one shell oracle snippet with a scrubbed environment:
/// `env_clear` plus `LC_ALL=C`, `PATH`, `TMPDIR`, then `set` vars
/// (single `env` entries, never `envs`) and `remove` vars.
/// Returns (exit code, stdout, stderr).
fn shell_oracle(set: &[(&str, &str)], remove: &[&str], body: &str) -> (i32, String, String) {
    let mut cmd = Command::new(bash_bin());
    cmd.arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(body)
        .arg("dot-test-sh")
        .arg(crate_root());
    cmd.env_clear();
    cmd.env("LC_ALL", "C");
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        if !tmpdir.is_empty() {
            cmd.env("TMPDIR", tmpdir);
        }
    }
    for (key, value) in set {
        cmd.env(key, value);
    }
    for key in remove {
        cmd.env_remove(key);
    }
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn oracle");
    (
        output.status.code().unwrap_or(99),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Run git for fixtures with a pinned identity (plus no-signing
/// insurance against ambient `commit.gpgsign` configs).
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} in {}", repo.display());
}

/// `git init -b main` plus one empty commit, for fixture repos.
fn git_init_seeded(repo: &Path) {
    std::fs::create_dir_all(repo).expect("fixture dir");
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["commit", "-q", "--allow-empty", "-m", "seed"]);
}

/// Newline-join helper for passing shell arrays through the
/// environment (records never contain newlines, like the shell
/// herestrings the checks read from).
fn join_lines(values: &[&str]) -> String {
    values.join("\n")
}

/// The renderer itself, byte-compared against the live `_dr_*`
/// emitters with and without details.
#[test]
fn render_matches_emitters_byte_for_byte() {
    let (code, stdout, _) = shell_oracle(
        &[],
        &[],
        "set -u\n. \"$1/lib/dot/doctor/runtime.sh\"\n_dr_section 'Heads'\n_dr_ok 'fine'\n_dr_ok 'fine detail' 'dee'\n_dr_warn 'careful'\n_dr_warn 'careful detail' 'dee'\n_dr_fail 'broken'\n_dr_fail 'broken detail' 'dee'\n_dr_skip 'later'\n_dr_skip 'later detail' 'dee'\n",
    );
    assert_eq!(code, 0, "oracle failed");
    let records = vec![
        Record::section("Heads"),
        Record::ok("fine", None),
        Record::ok("fine detail", Some("dee".to_string())),
        Record::warn("careful", None),
        Record::warn("careful detail", Some("dee".to_string())),
        Record::fail("broken", None),
        Record::fail("broken detail", Some("dee".to_string())),
        Record::skip("later", None),
        Record::skip("later detail", Some("dee".to_string())),
    ];
    assert_eq!(render(&records), stdout, "renderer diverged");
}

/// Lock oracle prelude: real runtime, lock, and check modules.
const LOCK_PRELUDE: &str = concat!(
    "set -u\n",
    ". \"$1/lib/dot/doctor/runtime.sh\"\n",
    ". \"$1/lib/dot/update-lock.sh\"\n",
    ". \"$1/lib/dot/doctor/lock.sh\"\n",
);

/// Run the shell lock check under isolated XDG/HOME roots.
fn shell_lock(state: &Path, home: &Path, extra: &[(&str, &str)], remove: &[&str]) -> (i32, String) {
    let mut set = vec![
        ("XDG_STATE_HOME", state.to_string_lossy().into_owned()),
        ("HOME", home.to_string_lossy().into_owned()),
    ];
    for (key, value) in extra {
        set.push((key, (*value).to_string()));
    }
    let refs: Vec<(&str, &str)> = set
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let (code, stdout, _) = shell_oracle(
        &refs,
        remove,
        &format!("{LOCK_PRELUDE}_dr_check_update_lock\n"),
    );
    (code, stdout)
}

fn lock_dir_of(state: &Path) -> PathBuf {
    state.join("dot").join("update.lock.d")
}

/// Clear lock: nothing at the lock path on either side.
#[test]
fn lock_clear_matches() {
    let scratch = TempDir::new("doctor-lock-clear").expect("scratch");
    let state = scratch.path().join("state");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let (code, stdout) = shell_lock(&state, &home, &[], &[]);
    assert_eq!(code, 0, "oracle failed");
    let rust = check_update_lock(Some(&lock_dir_of(&state)));
    assert_eq!(render(&rust), stdout, "lock clear diverged");
}

/// Unresolvable lock path: scrubbed HOME/XDG on the shell side,
/// `None` on the Rust side.
#[test]
fn lock_unresolvable_matches() {
    let (code, stdout, _) = shell_oracle(
        &[],
        &[
            "HOME",
            "XDG_STATE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
        ],
        &format!("{LOCK_PRELUDE}_dr_check_update_lock\n"),
    );
    assert_eq!(code, 0, "oracle failed");
    assert!(stdout.contains("cannot be resolved"), "{stdout:?}");
    let rust = check_update_lock(None);
    assert_eq!(render(&rust), stdout, "lock unresolvable diverged");
}

/// A regular file (and a symlink) at the lock path is unsafe.
#[test]
fn lock_unsafe_path_matches() {
    let scratch = TempDir::new("doctor-lock-unsafe").expect("scratch");
    // Regular file at the lock path.
    let file_state = scratch.path().join("file-state");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let dir = lock_dir_of(&file_state);
    std::fs::create_dir_all(dir.parent().expect("parent")).expect("parent");
    std::fs::write(&dir, b"not a dir").expect("plant file");
    let (code, stdout) = shell_lock(&file_state, &home, &[], &[]);
    assert_eq!(code, 0, "oracle failed");
    let rust = check_update_lock(Some(&dir));
    assert_eq!(render(&rust), stdout, "lock file-path diverged");

    // Symlink at the lock path (even to a real directory).
    let link_state = scratch.path().join("link-state");
    let target = scratch.path().join("target");
    std::fs::create_dir_all(&target).expect("target");
    let link = lock_dir_of(&link_state);
    std::fs::create_dir_all(link.parent().expect("parent")).expect("parent");
    std::os::unix::fs::symlink(&target, &link).expect("plant symlink");
    let (code, stdout) = shell_lock(&link_state, &home, &[], &[]);
    assert_eq!(code, 0, "oracle failed");
    let rust = check_update_lock(Some(&link));
    assert_eq!(render(&rust), stdout, "lock symlink-path diverged");
}

/// Fresh ownerless lock dir: initializing; aged: incomplete.
#[test]
fn lock_initializing_vs_incomplete_matches() {
    let scratch = TempDir::new("doctor-lock-aging").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");

    let fresh_state = scratch.path().join("fresh");
    let fresh_dir = lock_dir_of(&fresh_state);
    std::fs::create_dir_all(&fresh_dir).expect("fresh lock");
    let (code, stdout) = shell_lock(&fresh_state, &home, &[], &[]);
    assert_eq!(code, 0, "oracle failed");
    let rust = check_update_lock(Some(&fresh_dir));
    assert_eq!(render(&rust), stdout, "lock initializing diverged");

    let aged_state = scratch.path().join("aged");
    let aged_dir = lock_dir_of(&aged_state);
    std::fs::create_dir_all(&aged_dir).expect("aged lock");
    let status = Command::new("touch")
        .arg("-t")
        .arg("200001010000")
        .arg(&aged_dir)
        .status()
        .expect("touch");
    assert!(status.success());
    let (code, stdout) = shell_lock(&aged_state, &home, &[], &[]);
    assert_eq!(code, 0, "oracle failed");
    let rust = check_update_lock(Some(&aged_dir));
    assert_eq!(render(&rust), stdout, "lock incomplete diverged");
}

/// Live owner (a real Rust guard held by this process): running
/// with the same pid on both sides. Dead owner: stale.
#[test]
fn lock_owner_live_vs_stale_matches() {
    let scratch = TempDir::new("doctor-lock-owner").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let log = dot::log::Log::new(false, false);

    let live_state = scratch.path().join("live");
    let mut sink = Vec::new();
    let guard = dot::update_lock::acquire(&live_state, false, &log, None, &mut sink)
        .expect("acquire fixture lock");
    assert!(sink.is_empty());
    let (code, stdout) = shell_lock(&live_state, &home, &[], &[]);
    assert_eq!(code, 0, "oracle failed");
    let rust = check_update_lock(Some(&lock_dir_of(&live_state)));
    assert_eq!(render(&rust), stdout, "lock running diverged");
    assert!(
        stdout.contains(&format!("pid {}", std::process::id())),
        "{stdout:?}"
    );
    drop(guard);

    let stale_state = scratch.path().join("stale");
    let stale_dir = lock_dir_of(&stale_state);
    std::fs::create_dir_all(&stale_dir).expect("stale lock");
    std::fs::write(
        stale_dir.join("owner"),
        "pid\t42424242\nstart\tproc:1\ntoken\tstale.0.0\n",
    )
    .expect("stale owner");
    let (code, stdout) = shell_lock(&stale_state, &home, &[], &[]);
    assert_eq!(code, 0, "oracle failed");
    let rust = check_update_lock(Some(&stale_dir));
    assert_eq!(render(&rust), stdout, "lock stale diverged");
}

/// Merges oracle prelude: real runtime, trust, specs, and check
/// modules. `_dot_extensions_enabled` is a two-line copy of the
/// `config.sh` predicate (sourcing `config.sh` would run its
/// parser side effects, including unsetting
/// `DOT_SHDEPS_UPDATE_POLICY`).
const MERGES_PRELUDE: &str = concat!(
    "set -u\n",
    ". \"$1/lib/dot/doctor/runtime.sh\"\n",
    ". \"$1/lib/dot/extension-trust.sh\"\n",
    ". \"$1/lib/dot/merges.sh\"\n",
    ". \"$1/lib/dot/doctor/merges.sh\"\n",
    "_dot_extensions_enabled() { [[ ${DOT_EXTENSION_API:-} == 1 && -n ${DOT_EXTENSIONS_DIR:-} ]]; }\n",
);

/// Run the shell merges check; `extra` is prepended (stubs).
fn shell_merges(set: &[(&str, &str)], remove: &[&str], extra: &str) -> (i32, String, String) {
    shell_oracle(
        set,
        remove,
        &format!("{MERGES_PRELUDE}{extra}_dr_check_merges\n"),
    )
}

/// Capture the live `_merge_hook_specs` stdout for the Rust
/// `spec_count` input (`wc -l` counts `\n` bytes).
fn shell_specs(ext: &Path) -> (i32, String) {
    let ext_text = ext.to_string_lossy().into_owned();
    let (code, stdout, _) = shell_oracle(
        &[("DOT_EXTENSIONS_DIR", ext_text.as_str())],
        &[],
        &format!("{MERGES_PRELUDE}DOT_EXTENSION_API=1\n_merge_hook_specs\n"),
    );
    (code, stdout)
}

/// One trusted hook fixture (owned, group/world-writable-bit
/// free, valid identity).
fn hook_fixture(dir: &Path, name: &str) {
    std::fs::write(dir.join(name), "#!/bin/sh\necho hi\n").expect("hook fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o644))
            .expect("chmod hook");
    }
}

fn merges_case(enabled: bool, ext: &Path, spec_count: Option<usize>, extra: &str) {
    let ext_text = ext.to_string_lossy().into_owned();
    let refs: Vec<(&str, &str)> = if enabled {
        vec![
            ("DOT_EXTENSION_API", "1"),
            ("DOT_EXTENSIONS_DIR", ext_text.as_str()),
        ]
    } else {
        Vec::new()
    };
    let (code, stdout, _) = shell_merges(&refs, &[], extra);
    assert_eq!(code, 0, "oracle failed");
    let rust = check_merges(&MergeInputs {
        enabled,
        extensions_dir: ext.to_string_lossy().into_owned(),
        spec_count,
    });
    assert_eq!(render(&rust), stdout, "merges diverged");
}

/// Disabled extensions skip without touching the filesystem.
#[test]
fn merges_disabled_matches() {
    let scratch = TempDir::new("doctor-merges-off").expect("scratch");
    merges_case(false, &scratch.path().join("ext"), None, "");
}

/// Absent merge-hooks.d skips as none configured.
#[test]
fn merges_absent_root_matches() {
    let scratch = TempDir::new("doctor-merges-absent").expect("scratch");
    let ext = scratch.path().join("ext");
    std::fs::create_dir_all(&ext).expect("ext");
    let (spec_code, specs) = shell_specs(&ext);
    assert_eq!(spec_code, 0);
    assert_eq!(specs.bytes().filter(|b| *b == b'\n').count(), 0);
    merges_case(true, &ext, Some(0), "");
}

/// A file (or symlink) at the root is unavailable.
#[test]
fn merges_unavailable_root_matches() {
    let scratch = TempDir::new("doctor-merges-unavail").expect("scratch");
    let ext = scratch.path().join("ext");
    std::fs::create_dir_all(&ext).expect("ext");
    let root = ext.join("merge-hooks.d");
    std::fs::write(&root, b"not a dir").expect("plant file");
    merges_case(true, &ext, Some(0), "");

    std::fs::remove_file(&root).expect("clear");
    let target = scratch.path().join("target");
    std::fs::create_dir_all(&target).expect("target");
    std::os::unix::fs::symlink(&target, &root).expect("plant symlink");
    merges_case(true, &ext, Some(0), "");
}

/// Empty hooks dir: zero specs, none configured.
#[test]
fn merges_empty_dir_matches() {
    let scratch = TempDir::new("doctor-merges-empty").expect("scratch");
    let ext = scratch.path().join("ext");
    std::fs::create_dir_all(ext.join("merge-hooks.d")).expect("hooks dir");
    let (spec_code, specs) = shell_specs(&ext);
    assert_eq!(spec_code, 0);
    assert_eq!(specs.bytes().filter(|b| *b == b'\n').count(), 0);
    merges_case(true, &ext, Some(0), "");
}

/// Two valid hooks inventory as `2 hook(s)`.
#[test]
fn merges_two_hooks_matches() {
    let scratch = TempDir::new("doctor-merges-two").expect("scratch");
    let ext = scratch.path().join("ext");
    let hooks = ext.join("merge-hooks.d");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    hook_fixture(&hooks, "10-foo.sh");
    hook_fixture(&hooks, "20-bar.serial.sh");
    let (spec_code, specs) = shell_specs(&ext);
    assert_eq!(spec_code, 0);
    let count = specs.bytes().filter(|b| *b == b'\n').count();
    assert_eq!(count, 2, "fixture inventory: {specs:?}");
    merges_case(true, &ext, Some(count), "");
}

/// A failing `tr` fails the inventory pipeline: the invalid
/// branch. (Live inventory defects still exit 0 through `sort`,
/// so only tool failure reaches this arm.)
#[test]
fn merges_invalid_inventory_matches() {
    let scratch = TempDir::new("doctor-merges-invalid").expect("scratch");
    let ext = scratch.path().join("ext");
    std::fs::create_dir_all(ext.join("merge-hooks.d")).expect("hooks dir");
    merges_case(true, &ext, None, "tr() { return 1; }\n");
}

/// Lifecycle oracle prelude: real check module plus stubbed
/// load/deactivation/enabled boundaries driven by the
/// environment. `split_lines` fills `set -u` arrays from
/// newline-joined values (empty stays empty).
const LIFECYCLE_PRELUDE: &str = concat!(
    "set -u\n",
    "split_lines() { local -n _out=$1; _out=(); [[ -n ${2:-} ]] || return 0; mapfile -t _out <<<\"$2\"; }\n",
    ". \"$1/lib/dot/doctor/runtime.sh\"\n",
    ". \"$1/lib/dot/doctor/overlays.sh\"\n",
    "_dot_profile_lifecycle_load() { split_lines DOT_PROFILE_LIFECYCLE_RECORDS \"$LIFECYCLE_RECORDS\"; return \"$LOAD_RC\"; }\n",
    "_dot_profile_deactivation_script() { [[ $DEACT_BAD == *$'\\n'\"$1\"$'\\n'* ]] && return 1; REPLY=deact-ok; return 0; }\n",
    "_dot_extensions_enabled() { [[ ${EXT_ENABLED:-0} == 1 ]]; }\n",
);

/// Split a newline-joined env value like `split_lines` does.
fn env_list(value: &str) -> Vec<String> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split('\n').map(str::to_string).collect()
    }
}

#[allow(clippy::too_many_arguments)]
fn lifecycle_case(
    present: &str,
    load_rc: &str,
    eligible: &str,
    active: &str,
    records: &str,
    ext_enabled: &str,
    deact_bad: &str,
) {
    let (code, stdout, _) = shell_oracle(
        &[
            ("DOT_PROFILES_PRESENT", present),
            ("LOAD_RC", load_rc),
            ("ELIGIBLE", eligible),
            ("ACTIVE", active),
            ("LIFECYCLE_RECORDS", records),
            ("EXT_ENABLED", ext_enabled),
            ("DEACT_BAD", deact_bad),
        ],
        &[],
        &format!(
            "{LIFECYCLE_PRELUDE}split_lines ELIGIBLE_OVERLAY_NAMES \"$ELIGIBLE\"\nsplit_lines ACTIVE_OVERLAYS \"$ACTIVE\"\n_dr_check_profile_lifecycle\n"
        ),
    );
    assert_eq!(code, 0, "oracle failed");
    let bad: Vec<String> = env_list(deact_bad)
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect();
    let rust = check_profile_lifecycle(&LifecycleInputs {
        profiles_present: present == "1",
        load_ok: load_rc == "0",
        eligible: env_list(eligible),
        active: env_list(active),
        records: env_list(records),
        extensions_enabled: ext_enabled == "1",
        deactivation_ok: &|record| !bad.iter().any(|line| line == record),
    });
    assert_eq!(render(&rust), stdout, "lifecycle diverged");
}

/// Profiles absent: silent on both sides (no section here).
#[test]
fn lifecycle_absent_is_silent() {
    lifecycle_case("0", "0", "", "", "", "0", "");
}

/// Ledger load failure fails the lifecycle state.
#[test]
fn lifecycle_load_failure_matches() {
    lifecycle_case("1", "1", "", "", "", "1", "");
}

/// Eligible, active, authorized: clean bill.
#[test]
fn lifecycle_eligible_active_ok_matches() {
    let record = "a|/p|u|d|false|git";
    lifecycle_case("1", "0", "a", record, record, "1", "");
}

/// Active deactivation authority unsafe, then the clean-bill ok
/// (pending stays empty): the shell emits both records.
#[test]
fn lifecycle_active_authority_unsafe_matches() {
    let record = "a|/p|u|d|false|git";
    lifecycle_case("1", "0", "a", record, record, "1", &format!("\n{record}\n"));
}

/// Retained (eligible, inactive) authority missing warns, then ok.
#[test]
fn lifecycle_retained_authority_warns() {
    let record = "a|/p|u|d|true|git";
    lifecycle_case("1", "0", "a", "", record, "1", &format!("\n{record}\n"));
}

/// Pending deactivation with extensions disabled fails twice.
#[test]
fn lifecycle_pending_extensions_disabled_matches() {
    let record = "z|/p|u|d|false|git";
    lifecycle_case("1", "0", "", "", record, "0", "");
}

/// Pending with extensions enabled but unsafe authority.
#[test]
fn lifecycle_pending_retiring_unsafe_matches() {
    let record = "z|/p|u|d|false|git";
    lifecycle_case("1", "0", "", "", record, "1", &format!("\n{record}\n"));
}

/// Pending with a usable authority fails only the pending rollup.
#[test]
fn lifecycle_pending_healthy_matches() {
    let record = "z|/p|u|d|false|git";
    lifecycle_case("1", "0", "", "", record, "1", "");
}

/// Mixed eligible plus pending records.
#[test]
fn lifecycle_mixed_matches() {
    let eligible_rec = "a|/pa|u|d|false|git";
    let pending_rec = "z|/pz|u|d|false|git";
    lifecycle_case(
        "1",
        "0",
        "a",
        eligible_rec,
        &join_lines(&[eligible_rec, pending_rec]),
        "1",
        "",
    );
}

/// Overlays oracle prelude: real runtime, paths, repos helpers,
/// and check modules; only the trust-policy probes are stubbed
/// (lifecycle load, deactivation authority, extensions gate,
/// local-source validation), driven by the environment like the
/// lifecycle harness.
const OVERLAYS_PRELUDE: &str = concat!(
    "set -u\n",
    "split_lines() { local -n _out=$1; _out=(); [[ -n ${2:-} ]] || return 0; mapfile -t _out <<<\"$2\"; }\n",
    ". \"$1/lib/dot/doctor/runtime.sh\"\n",
    ". \"$1/lib/dot/doctor/paths.sh\"\n",
    ". \"$1/lib/dot/repos/overlays.sh\"\n",
    ". \"$1/lib/dot/repos/config.sh\"\n",
    ". \"$1/lib/dot/doctor/overlays.sh\"\n",
    "_dot_profile_lifecycle_load() { split_lines DOT_PROFILE_LIFECYCLE_RECORDS \"$LIFECYCLE_RECORDS\"; return \"$LOAD_RC\"; }\n",
    "_dot_profile_deactivation_script() { [[ $DEACT_BAD == *$'\\n'\"$1\"$'\\n'* ]] && return 1; REPLY=deact-ok; return 0; }\n",
    "_dot_extensions_enabled() { [[ ${EXT_ENABLED:-0} == 1 ]]; }\n",
    "_overlay_local_source_validate() { if [[ $LOCAL_VALID_BAD == *$'\\n'\"$1\"$'\\n'* ]]; then REPLY=\"$LOCAL_VALID_REPLY\"; return 1; fi; REPLY=\"\"; return 0; }\n",
);

/// One overlays scenario: every shell global as a field.
#[derive(Default)]
struct OverlayScenario {
    home: String,
    config_error: String,
    profiles_present: bool,
    user: String,
    host: String,
    selected: String,
    selection_state: String,
    included: Vec<String>,
    phase_one: Vec<String>,
    selectors: Vec<String>,
    eligible: Vec<String>,
    active: Vec<String>,
    lifecycle_records: Vec<String>,
    load_ok: bool,
    extensions_enabled: bool,
    deact_bad: Vec<String>,
    configured_count: usize,
    manifest: String,
    discovery_error: String,
    active_records: Vec<String>,
    overlay_lifecycle: Vec<String>,
    local_bad: Vec<String>,
    local_reply: String,
}

fn overlays_case(scenario: &OverlayScenario) {
    let s = scenario;
    let included = s.included.join("\n");
    let phase_one = s.phase_one.join("\n");
    let selectors = s.selectors.join("\n");
    let eligible = s.eligible.join("\n");
    let active = s.active.join("\n");
    let lifecycle_records = s.lifecycle_records.join("\n");
    let deact_bad = if s.deact_bad.is_empty() {
        String::new()
    } else {
        format!("\n{}\n", s.deact_bad.join("\n"))
    };
    let active_records = s.active_records.join("\n");
    let overlay_lifecycle = s.overlay_lifecycle.join("\n");
    let local_bad = if s.local_bad.is_empty() {
        String::new()
    } else {
        format!("\n{}\n", s.local_bad.join("\n"))
    };
    let conf_count = s.configured_count.to_string();
    let (code, stdout, _) = shell_oracle(
        &[
            ("HOME", s.home.as_str()),
            ("DOT_PROFILE_CONFIGURATION_ERROR", s.config_error.as_str()),
            (
                "DOT_PROFILES_PRESENT",
                if s.profiles_present { "1" } else { "0" },
            ),
            ("DOT_PROFILE_CURRENT_USER", s.user.as_str()),
            ("DOT_PROFILE_CURRENT_HOST", s.host.as_str()),
            ("SELECTED_PROFILE", s.selected.as_str()),
            ("DOT_PROFILE_SELECTION_STATE", s.selection_state.as_str()),
            ("INCLUDED", included.as_str()),
            ("PHASEONE", phase_one.as_str()),
            ("SELECTORS", selectors.as_str()),
            ("ELIGIBLE", eligible.as_str()),
            ("ACTIVE", active.as_str()),
            ("LIFECYCLE_RECORDS", lifecycle_records.as_str()),
            ("LOAD_RC", if s.load_ok { "0" } else { "1" }),
            ("EXT_ENABLED", if s.extensions_enabled { "1" } else { "0" }),
            ("DEACT_BAD", deact_bad.as_str()),
            ("CONF_COUNT", conf_count.as_str()),
            ("DOT_OVERLAY_MANIFEST", s.manifest.as_str()),
            ("DOT_OVERLAY_DISCOVERY_ERROR", s.discovery_error.as_str()),
            ("ACTIVE_OVERLAYS_IN", active_records.as_str()),
            ("OVERLAY_LIFECYCLE", overlay_lifecycle.as_str()),
            ("LOCAL_VALID_BAD", local_bad.as_str()),
            ("LOCAL_VALID_REPLY", s.local_reply.as_str()),
        ],
        &[],
        &format!(
            "{OVERLAYS_PRELUDE}split_lines ELIGIBLE_OVERLAY_NAMES \"$ELIGIBLE\"\n\
             split_lines ACTIVE_OVERLAYS \"$ACTIVE_OVERLAYS_IN\"\n\
             split_lines INCLUDED_PROFILES \"$INCLUDED\"\n\
             split_lines PHASE_ONE_SELECTED_OVERLAY_NAMES \"$PHASEONE\"\n\
             split_lines DOT_PROFILE_SELECTOR_RECORDS \"$SELECTORS\"\n\
             split_lines DOT_OVERLAY_LIFECYCLE \"$OVERLAY_LIFECYCLE\"\n\
             CONFIGURED_OVERLAY_NAMES=()\n\
             for ((i = 0; i < CONF_COUNT; i++)); do CONFIGURED_OVERLAY_NAMES+=(\"conf$i\"); done\n\
             _dr_check_overlays\n"
        ),
    );
    assert_eq!(code, 0, "oracle failed");
    let rust = check_overlays(&OverlayInputs {
        home: s.home.as_str(),
        profile_config_error: Some(s.config_error.as_str()),
        profiles_present: s.profiles_present,
        profile_user: Some(s.user.as_str()),
        profile_host: Some(s.host.as_str()),
        selected_profile: Some(s.selected.as_str()),
        selection_state: Some(s.selection_state.as_str()),
        included_profiles: s.included.clone(),
        phase_one: s.phase_one.clone(),
        selectors: s.selectors.clone(),
        lifecycle: LifecycleInputs {
            profiles_present: s.profiles_present,
            load_ok: s.load_ok,
            eligible: s.eligible.clone(),
            active: s.active.clone(),
            records: s.lifecycle_records.clone(),
            extensions_enabled: s.extensions_enabled,
            deactivation_ok: &|record| !s.deact_bad.iter().any(|line| line == record),
        },
        configured_count: s.configured_count,
        manifest: s.manifest.clone(),
        discovery_error: Some(s.discovery_error.as_str()),
        active_records: s.active_records.clone(),
        overlay_lifecycle: s.overlay_lifecycle.clone(),
        local_validate: &|path| {
            if s.local_bad.iter().any(|line| line == path) {
                Err(s.local_reply.clone())
            } else {
                Ok(())
            }
        },
    });
    assert_eq!(render(&rust), stdout, "overlays diverged");
}

/// Seed a git overlay fixture with the given origin URL.
fn overlay_repo(dir: &Path, url: &str) {
    git_init_seeded(dir);
    git(dir, &["remote", "add", "origin", url]);
}

/// Profile configuration error short-circuits the header.
#[test]
fn overlays_config_error_matches() {
    let scratch = TempDir::new("doctor-ov-config").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        config_error: "bad profile: x".to_string(),
        manifest: scratch
            .path()
            .join("missing-manifest")
            .to_string_lossy()
            .into_owned(),
        load_ok: true,
        ..Default::default()
    });
}

/// Legacy discovery without profiles.
#[test]
fn overlays_legacy_matches() {
    let scratch = TempDir::new("doctor-ov-legacy").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        manifest: scratch
            .path()
            .join("missing-manifest")
            .to_string_lossy()
            .into_owned(),
        load_ok: true,
        ..Default::default()
    });
}

/// Full profile header: identity, selection, inclusions,
/// phase-one, matched selectors, nested lifecycle.
#[test]
fn overlays_profile_header_matches() {
    let scratch = TempDir::new("doctor-ov-header").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        profiles_present: true,
        user: "ada".to_string(),
        host: "host".to_string(),
        selected: "work".to_string(),
        selection_state: "exact".to_string(),
        included: vec!["base".to_string(), "extra".to_string()],
        phase_one: vec!["p1".to_string()],
        selectors: vec![
            "root|/r/sel|u|h|prof|false".to_string(),
            "root|/r/a|u|h|pa|true".to_string(),
            "local|/l/b|u|h|pb|true".to_string(),
            "personal|/p/c|u|h|pc|true".to_string(),
            "custom|/x/d|u|h|pd|true".to_string(),
        ],
        load_ok: true,
        manifest: scratch
            .path()
            .join("missing-manifest")
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    });
}

/// Discovery error plus empty configuration.
#[test]
fn overlays_discovery_error_matches() {
    let scratch = TempDir::new("doctor-ov-discover").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        discovery_error: "bad overlay: y".to_string(),
        manifest: scratch
            .path()
            .join("missing-manifest")
            .to_string_lossy()
            .into_owned(),
        load_ok: true,
        ..Default::default()
    });
}

/// No descriptors but an (empty) manifest still runs the link pass.
#[test]
fn overlays_empty_manifest_matches() {
    let scratch = TempDir::new("doctor-ov-emptyman").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let manifest = scratch.path().join("manifest");
    std::fs::write(&manifest, b"").expect("empty manifest");
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        manifest: manifest.to_string_lossy().into_owned(),
        load_ok: true,
        ..Default::default()
    });
}

/// Lifecycle state matrix: skips, fails, missing record,
/// unknown state, and one healthy git overlay.
#[test]
fn overlays_lifecycle_matrix_matches() {
    let scratch = TempDir::new("doctor-ov-matrix").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let repo = scratch.path().join("repo");
    overlay_repo(&repo, "file:///overlays/o7");
    let repo_text = repo.to_string_lossy().into_owned();
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        configured_count: 2,
        manifest: scratch
            .path()
            .join("missing-manifest")
            .to_string_lossy()
            .into_owned(),
        load_ok: true,
        active_records: vec![format!("o7|{repo_text}|file:///overlays/o7|d|false|git")],
        overlay_lifecycle: vec![
            "o1|not-selected|x".to_string(),
            "o2|selected-ineligible|x".to_string(),
            "o3|selected-optional-unavailable|x".to_string(),
            "o4|selected-unavailable|x".to_string(),
            "o5|active|x".to_string(),
            "o6|bogus|x".to_string(),
            "o7|active|x".to_string(),
        ],
        ..Default::default()
    });
}

/// Local (`sync=none`) sources: available, fallback-diagnostic
/// failure, and custom-diagnostic failure.
#[test]
fn overlays_local_sources_matches() {
    let scratch = TempDir::new("doctor-ov-local").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let p1 = scratch.path().join("p1");
    let p2 = scratch.path().join("p2");
    let bad = vec![p2.to_string_lossy().into_owned()];
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        configured_count: 2,
        manifest: scratch
            .path()
            .join("missing-manifest")
            .to_string_lossy()
            .into_owned(),
        load_ok: true,
        active_records: vec![
            format!("n1|{}|u|d|false|none", p1.to_string_lossy()),
            format!("n2|{}|u|d|false|none", p2.to_string_lossy()),
        ],
        overlay_lifecycle: vec!["n1|active|x".to_string(), "n2|active|x".to_string()],
        local_bad: bad,
        ..Default::default()
    });
    // Same failure with a custom REPLY diagnostic.
    let p2b = scratch.path().join("p2");
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        configured_count: 1,
        manifest: scratch
            .path()
            .join("missing-manifest")
            .to_string_lossy()
            .into_owned(),
        load_ok: true,
        active_records: vec![format!("n2|{}|u|d|false|none", p2b.to_string_lossy())],
        overlay_lifecycle: vec!["n2|active|x".to_string()],
        local_bad: vec![p2b.to_string_lossy().into_owned()],
        local_reply: "custom-diag".to_string(),
        ..Default::default()
    });
}

/// Cloned, drifted, missing, optional-missing, and non-worktree
/// git sources.
#[test]
fn overlays_git_sources_matches() {
    let scratch = TempDir::new("doctor-ov-git").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let same = scratch.path().join("same");
    overlay_repo(&same, "file:///overlays/same");
    let drift = scratch.path().join("drift");
    overlay_repo(&drift, "file:///overlays/other");
    let plain = scratch.path().join("plain");
    std::fs::create_dir_all(&plain).expect("plain dir");
    let missing = scratch.path().join("missing");
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        configured_count: 5,
        manifest: scratch
            .path()
            .join("missing-manifest")
            .to_string_lossy()
            .into_owned(),
        load_ok: true,
        active_records: vec![
            format!(
                "c1|{}|file:///overlays/same|d|false|git",
                same.to_string_lossy()
            ),
            format!(
                "c2|{}|file:///overlays/drift|d|false|git",
                drift.to_string_lossy()
            ),
            format!(
                "c3|{}|file:///overlays/c3|d|false|git",
                missing.to_string_lossy()
            ),
            format!(
                "c4|{}|file:///overlays/c4|d|true|git",
                missing.to_string_lossy()
            ),
            format!(
                "c5|{}|file:///overlays/c5|d|false|git",
                plain.to_string_lossy()
            ),
        ],
        overlay_lifecycle: vec![
            "c1|active|x".to_string(),
            "c2|active|x".to_string(),
            "c3|active|x".to_string(),
            "c4|active|x".to_string(),
            "c5|active|x".to_string(),
        ],
        ..Default::default()
    });
}

/// Sync spelling corners: a seventh field sticks to `sync`
/// (not `none`, so the worktree arm runs), an empty `sync`
/// defaults to `git`, and `optional` must read exactly `true`.
#[test]
fn overlays_sync_spelling_matches() {
    let scratch = TempDir::new("doctor-ov-sync").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let repo = scratch.path().join("repo");
    overlay_repo(&repo, "file:///overlays/s");
    let repo_text = repo.to_string_lossy().into_owned();
    let missing = scratch.path().join("missing");
    let missing_text = missing.to_string_lossy().into_owned();
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        configured_count: 3,
        manifest: scratch
            .path()
            .join("missing-manifest")
            .to_string_lossy()
            .into_owned(),
        load_ok: true,
        active_records: vec![
            format!("s|{repo_text}|file:///overlays/s|d|false|git|EXTRA"),
            format!("s2|{repo_text}|file:///overlays/s|d|false|"),
            format!("s3|{missing_text}|file:///overlays/s3|d|True|git"),
        ],
        overlay_lifecycle: vec![
            "s|active|x".to_string(),
            "s2|active|x".to_string(),
            "s3|active|x".to_string(),
        ],
        ..Default::default()
    });
}

/// Manifest ownership pass: healthy two- and three-column links,
/// an inexact fallback through a `/./` spelling, one exact-target
/// drift, an unknown owner, a dangling link, a non-link, and one
/// unparseable line: five issues.
#[test]
fn overlays_manifest_issues_matches() {
    let scratch = TempDir::new("doctor-ov-manifest").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let opath = scratch.path().join("ohome");
    let ohome = opath.join("home");
    std::fs::create_dir_all(&ohome).expect("overlay home");
    let opath_text = opath.to_string_lossy().into_owned();
    for leaf in ["rel", "e", "f", "g2"] {
        std::fs::write(ohome.join(leaf), "x\n").expect("overlay leaf");
    }
    let elsewhere = scratch.path().join("elsewhere");
    std::fs::write(&elsewhere, "y\n").expect("drift target");
    let elsewhere_text = elsewhere.to_string_lossy().into_owned();
    std::os::unix::fs::symlink(format!("{opath_text}/home/rel"), home.join("rel"))
        .expect("healthy link");
    std::os::unix::fs::symlink(format!("{opath_text}/home/e"), home.join("e")).expect("exact link");
    std::os::unix::fs::symlink(format!("{opath_text}/home/./f"), home.join("f"))
        .expect("fallback link");
    std::os::unix::fs::symlink(&elsewhere_text, home.join("d")).expect("drift link");
    std::os::unix::fs::symlink(format!("{opath_text}/home/g2"), home.join("g2"))
        .expect("unknown-owner link");
    std::os::unix::fs::symlink("/nonexistent-target-xyz", home.join("broke"))
        .expect("dangling link");
    std::fs::write(home.join("plain"), "z\n").expect("non-link");
    let manifest = scratch.path().join("manifest");
    let body = format!(
        "rel\to\ne\to\t{opath_text}/home/e\nd\to\t{elsewhere_text}\nf\to\ng2\tghost\nbroke\to\nplain\to\ngarbage-no-tab\n"
    );
    std::fs::write(&manifest, body).expect("manifest");
    overlays_case(&OverlayScenario {
        home: home.to_string_lossy().into_owned(),
        configured_count: 1,
        manifest: manifest.to_string_lossy().into_owned(),
        load_ok: true,
        active_records: vec![format!("o|{opath_text}|u|d|false|none")],
        overlay_lifecycle: vec!["o|active|x".to_string()],
        ..Default::default()
    });
}

/// Jennings oracle: the bare `_dr_shdeps_binary` function only
/// needs the filesystem. Witness: `selected=<path>` or `rc=1`.
const SHDEPS_PRELUDE: &str = "set -u\n. \"$1/lib/dot/doctor/provider.sh\"\n";

/// Write an executable fixture file.
fn exec_fixture(path: &Path) {
    std::fs::write(path, "#!/bin/sh\necho jennings\n").expect("jennings fixture");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod +x");
}

fn shdeps_binary_case(shdepsw_bin: Option<&Path>, installer: &Path) {
    let installer_text = installer.to_string_lossy().into_owned();
    let mut set = vec![("INSTALLER", installer_text.as_str())];
    let bin_text;
    if let Some(bin) = shdepsw_bin {
        bin_text = bin.to_string_lossy().into_owned();
        set.push(("_SHDEPSW_BIN", bin_text.as_str()));
    }
    let remove = if shdepsw_bin.is_none() {
        vec!["_SHDEPSW_BIN"]
    } else {
        Vec::new()
    };
    let (code, stdout, _) = shell_oracle(
        &set,
        &remove,
        &format!(
            "{SHDEPS_PRELUDE}if _dr_shdeps_binary \"$INSTALLER\"; then printf 'selected=%s\\n' \"$REPLY\"; else printf 'rc=1\\n'; fi\n"
        ),
    );
    assert_eq!(code, 0, "oracle failed");
    let rust = shdeps_binary(shdepsw_bin, installer);
    let witnessed = match rust {
        Some(path) => format!("selected={}\n", path.display()),
        None => "rc=1\n".to_string(),
    };
    assert_eq!(witnessed, stdout, "shdeps binary diverged");
}

/// A pre-selected executable wins, even over installer siblings.
#[test]
fn shdeps_binary_preselected_wins() {
    let scratch = TempDir::new("doctor-shdeps-pre").expect("scratch");
    let bin = scratch.path().join("shdepsw");
    exec_fixture(&bin);
    let installer = scratch.path().join("install.sh");
    std::fs::write(&installer, "x\n").expect("installer");
    shdeps_binary_case(Some(&bin), &installer);
}

/// A pre-selected directory with the execute bit is selected
/// too: the shell only probes `-x` there.
#[test]
fn shdeps_binary_preselected_dir_matches() {
    let scratch = TempDir::new("doctor-shdeps-predir").expect("scratch");
    let dir = scratch.path().join("predir");
    std::fs::create_dir_all(&dir).expect("dir");
    let installer = scratch.path().join("install.sh");
    shdeps_binary_case(Some(&dir), &installer);
}

/// A non-executable pre-selection falls through to the
/// installer-relative candidates.
#[test]
fn shdeps_binary_falls_through() {
    let scratch = TempDir::new("doctor-shdeps-fall").expect("scratch");
    let dull = scratch.path().join("dull");
    std::fs::write(&dull, "x\n").expect("dull");
    let root = scratch.path().join("root");
    std::fs::create_dir_all(&root).expect("root");
    let installer = root.join("install.sh");
    std::fs::write(&installer, "x\n").expect("installer");
    // Plain sibling wins.
    let sibling = root.join("shdeps");
    exec_fixture(&sibling);
    shdeps_binary_case(Some(&dull), &installer);
    // Without the sibling, debug wins over release.
    std::fs::remove_file(&sibling).expect("clear sibling");
    let debug = root.join("target").join("debug").join("shdeps");
    std::fs::create_dir_all(debug.parent().expect("parent")).expect("debug dir");
    exec_fixture(&debug);
    let release = root.join("target").join("release").join("shdeps");
    std::fs::create_dir_all(release.parent().expect("parent")).expect("release dir");
    exec_fixture(&release);
    shdeps_binary_case(Some(&dull), &installer);
    // A symlink candidate is skipped even when executable.
    std::fs::remove_file(&debug).expect("clear debug");
    std::os::unix::fs::symlink(&release, &debug).expect("plant symlink");
    shdeps_binary_case(Some(&dull), &installer);
}

/// Nothing selectable reports `rc=1`, including a slash-free
/// installer spelling (root keeps the whole string).
#[test]
fn shdeps_binary_missing_matches() {
    let scratch = TempDir::new("doctor-shdeps-miss").expect("scratch");
    let installer = scratch.path().join("install.sh");
    shdeps_binary_case(None, &installer);
    shdeps_binary_case(None, Path::new("install.sh"));
}

/// Provider oracle prelude: real check module plus stubbed shdeps
/// selection/probe boundaries driven by the environment. The real
/// `_dr_shdeps_binary` resolves against fixture files.
const PROVIDER_PRELUDE: &str = concat!(
    "set -u\n",
    ". \"$1/lib/dot/doctor/runtime.sh\"\n",
    ". \"$1/lib/dot/doctor/paths.sh\"\n",
    ". \"$1/lib/dot/doctor/provider.sh\"\n",
    "_dot_shdeps_configure_env() { [[ ${CONF_OK:-0} == 1 ]]; }\n",
    "_dot_shdeps_development_checkout_valid() { [[ ${DEV_VALID:-0} == 1 ]]; }\n",
    "_dot_shdeps_installer() { [[ ${INST_OK:-0} == 1 ]] || return 1; REPLY=\"$INST_PATH\"; _DOT_SHDEPS_INSTALLER_SOURCE=\"$INST_SOURCE\"; }\n",
    "_dot_shdeps_lock_value() { local _v; case $1 in revision) _v=$LOCK_REV;; abi) _v=$LOCK_ABI;; *) return 1;; esac; [[ ${LOCK_RC:-0} == 1 ]] || return 1; printf '%s' \"$_v\"; }\n",
    "_dot_sanitized_git() { [[ ${DEV_REV_RC:-0} == 1 ]] || return 1; printf '%s' \"$DEV_REV\"; }\n",
    "_dot_shdeps_binary_abi_version() { [[ ${ABI_RC:-0} == 1 ]] || { REPLY=\"\"; return 1; }; REPLY=\"$ABI_REPLY\"; }\n",
);

/// One provider scenario: every shell global/helper outcome.
#[derive(Default)]
struct ProviderScenario {
    home: String,
    provider: String,
    provider_set: bool,
    policy: String,
    conf_ok: bool,
    dev_dir: String,
    dev_exists: bool,
    dev_valid: bool,
    inst_ok: bool,
    inst_path: String,
    inst_source: String,
    lock_rev: String,
    lock_rev_ok: bool,
    dev_rev: String,
    dev_rev_ok: bool,
    binary: String,
    binary_set: bool,
    abi_expected: String,
    abi_expected_ok: bool,
    abi_actual: String,
    abi_ok: bool,
    shdepsw_bin: String,
}

fn provider_case(scenario: &ProviderScenario) {
    let s = scenario;
    let mut set: Vec<(String, String)> = vec![
        ("HOME".to_string(), s.home.clone()),
        ("DOT_SHDEPS_UPDATE_POLICY".to_string(), s.policy.clone()),
        ("CONF_OK".to_string(), flag(s.conf_ok)),
        ("SHDEPS_GIT_DEV_DIR".to_string(), s.dev_dir.clone()),
        ("DEV_VALID".to_string(), flag(s.dev_valid)),
        ("INST_OK".to_string(), flag(s.inst_ok)),
        ("INST_PATH".to_string(), s.inst_path.clone()),
        ("INST_SOURCE".to_string(), s.inst_source.clone()),
        ("LOCK_REV".to_string(), s.lock_rev.clone()),
        // Revision and ABI read the same lock file, so they
        // succeed or fail together in reality.
        (
            "LOCK_RC".to_string(),
            flag(s.lock_rev_ok || s.abi_expected_ok),
        ),
        ("LOCK_ABI".to_string(), s.abi_expected.clone()),
        ("DEV_REV".to_string(), s.dev_rev.clone()),
        ("DEV_REV_RC".to_string(), flag(s.dev_rev_ok)),
        ("ABI_REPLY".to_string(), s.abi_actual.clone()),
        ("ABI_RC".to_string(), flag(s.abi_ok)),
        ("DOT_DEPENDENCY_PROVIDER".to_string(), s.provider.clone()),
    ];
    if !s.shdepsw_bin.is_empty() {
        set.push(("_SHDEPSW_BIN".to_string(), s.shdepsw_bin.clone()));
    }
    if s.dev_exists {
        let dev = format!("{}/shdeps", s.dev_dir);
        std::fs::create_dir_all(&dev).expect("dev checkout");
    }
    let refs: Vec<(&str, &str)> = set
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let remove = if s.provider_set {
        vec![]
    } else {
        vec!["DOT_DEPENDENCY_PROVIDER"]
    };
    let (code, stdout, _) = shell_oracle(
        &refs,
        &remove,
        &format!("{PROVIDER_PRELUDE}_dr_check_provider\n"),
    );
    assert_eq!(code, 0, "oracle failed");
    let installer = if s.inst_ok {
        Some(ProviderInstaller {
            path: s.inst_path.as_str(),
            source: s.inst_source.as_str(),
        })
    } else {
        None
    };
    // The shell `LOCK_ABI`/`LOCK_REV`/`DEV_REV` stubs fail closed
    // through `LOCK_RC`/`DEV_REV_RC`; mirror the `|| true` empties.
    let rust = check_provider(&ProviderInputs {
        home: s.home.as_str(),
        dependency_provider: if s.provider_set {
            Some(s.provider.as_str())
        } else {
            None
        },
        policy: s.policy.as_str(),
        configure_ok: s.conf_ok,
        dev_dir: s.dev_dir.as_str(),
        development_exists: std::fs::symlink_metadata(format!("{}/shdeps", s.dev_dir)).is_ok(),
        development_valid: s.dev_valid,
        installer,
        locked_revision: if s.lock_rev_ok {
            Some(s.lock_rev.as_str())
        } else {
            None
        },
        development_revision: if s.dev_rev_ok {
            Some(s.dev_rev.as_str())
        } else {
            None
        },
        binary: if s.binary_set {
            Some(s.binary.as_str())
        } else {
            None
        },
        expected_abi: if s.abi_expected_ok {
            Some(s.abi_expected.as_str())
        } else {
            None
        },
        actual_abi: if s.abi_ok {
            Some(s.abi_actual.as_str())
        } else {
            None
        },
    });
    assert_eq!(render(&rust), stdout, "provider diverged");
}

fn flag(value: bool) -> String {
    if value {
        "1".to_string()
    } else {
        "0".to_string()
    }
}

/// No (or empty) provider skips.
#[test]
fn provider_none_matches() {
    let scratch = TempDir::new("doctor-prov-none").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let home_text = home.to_string_lossy().into_owned();
    provider_case(&ProviderScenario {
        home: home_text.clone(),
        ..Default::default()
    });
    provider_case(&ProviderScenario {
        home: home_text,
        provider_set: true,
        ..Default::default()
    });
}

/// An unknown provider name fails.
#[test]
fn provider_unsupported_matches() {
    let scratch = TempDir::new("doctor-prov-unsup").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    provider_case(&ProviderScenario {
        home: home.to_string_lossy().into_owned(),
        provider: "conda".to_string(),
        provider_set: true,
        ..Default::default()
    });
}

/// A failing `configure_env` fails the provider after the policy.
#[test]
fn provider_configure_failure_matches() {
    let scratch = TempDir::new("doctor-prov-conf").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    provider_case(&ProviderScenario {
        home: home.to_string_lossy().into_owned(),
        provider: "shdeps".to_string(),
        provider_set: true,
        policy: "latest".to_string(),
        ..Default::default()
    });
}

/// A failing installer fails, warning first when a latest-policy
/// development checkout is invalid.
#[test]
fn provider_installer_failure_matches() {
    let scratch = TempDir::new("doctor-prov-inst").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let home_text = home.to_string_lossy().into_owned();
    let dev = scratch.path().join("git");
    // Latest policy with an existing but invalid checkout warns.
    provider_case(&ProviderScenario {
        home: home_text.clone(),
        provider: "shdeps".to_string(),
        provider_set: true,
        policy: "latest".to_string(),
        conf_ok: true,
        dev_dir: dev.to_string_lossy().into_owned(),
        dev_exists: true,
        ..Default::default()
    });
    // Pinned policy never evaluates the checkout: no warning.
    provider_case(&ProviderScenario {
        home: home_text,
        provider: "shdeps".to_string(),
        provider_set: true,
        conf_ok: true,
        dev_dir: dev.to_string_lossy().into_owned(),
        dev_exists: true,
        ..Default::default()
    });
}

/// Explicit source with matching ABI: the full ok chain.
#[test]
fn provider_explicit_abi_match_matches() {
    let scratch = TempDir::new("doctor-prov-expl").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let home_text = home.to_string_lossy().into_owned();
    let bin = home.join("shdeps");
    exec_fixture(&bin);
    let inst = home.join("install.sh");
    std::fs::write(&inst, "x\n").expect("installer");
    let inst_text = inst.to_string_lossy().into_owned();
    provider_case(&ProviderScenario {
        home: home_text,
        provider: "shdeps".to_string(),
        provider_set: true,
        conf_ok: true,
        dev_dir: scratch.path().join("git").to_string_lossy().into_owned(),
        inst_ok: true,
        inst_path: inst_text,
        inst_source: "explicit".to_string(),
        binary: "x".to_string(),
        binary_set: true,
        abi_expected: "7".to_string(),
        abi_expected_ok: true,
        abi_actual: "abi:7".to_string(),
        abi_ok: true,
        shdepsw_bin: bin.to_string_lossy().into_owned(),
        ..Default::default()
    });
}

/// Pinned-dev source under the latest policy reports the Dot-lock
/// checkout and compares revisions.
#[test]
fn provider_pinned_dev_revision_matches() {
    let scratch = TempDir::new("doctor-prov-pinned").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let home_text = home.to_string_lossy().into_owned();
    let bin = home.join("shdeps");
    exec_fixture(&bin);
    let inst = home.join("install.sh");
    let inst_text = inst.to_string_lossy().into_owned();
    // Revision matches the lock.
    provider_case(&ProviderScenario {
        home: home_text.clone(),
        provider: "shdeps".to_string(),
        provider_set: true,
        policy: "latest".to_string(),
        conf_ok: true,
        dev_dir: scratch.path().join("git").to_string_lossy().into_owned(),
        dev_exists: true,
        dev_valid: true,
        inst_ok: true,
        inst_path: inst_text.clone(),
        inst_source: "pinned-dev".to_string(),
        lock_rev: "abc123".to_string(),
        lock_rev_ok: true,
        dev_rev: "abc123".to_string(),
        dev_rev_ok: true,
        binary: "x".to_string(),
        binary_set: true,
        abi_expected: "7".to_string(),
        abi_expected_ok: true,
        abi_actual: "abi:7".to_string(),
        abi_ok: true,
        shdepsw_bin: bin.to_string_lossy().into_owned(),
    });
    // Revision differs: trusted unpinned.
    provider_case(&ProviderScenario {
        home: home_text,
        provider: "shdeps".to_string(),
        provider_set: true,
        policy: "latest".to_string(),
        conf_ok: true,
        dev_dir: scratch.path().join("git").to_string_lossy().into_owned(),
        dev_exists: true,
        dev_valid: true,
        inst_ok: true,
        inst_path: inst_text,
        inst_source: "pinned-dev".to_string(),
        lock_rev: "abc123".to_string(),
        lock_rev_ok: true,
        dev_rev: "def456".to_string(),
        dev_rev_ok: true,
        binary: "x".to_string(),
        binary_set: true,
        abi_expected: "7".to_string(),
        abi_expected_ok: true,
        abi_actual: "abi:7".to_string(),
        abi_ok: true,
        shdepsw_bin: bin.to_string_lossy().into_owned(),
    });
}

/// Latest-dev and managed sources, plus the unknown-source fail.
#[test]
fn provider_sources_matches() {
    let scratch = TempDir::new("doctor-prov-src").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let home_text = home.to_string_lossy().into_owned();
    let bin = home.join("shdeps");
    exec_fixture(&bin);
    let bin_text = bin.to_string_lossy().into_owned();
    let inst = home.join("install.sh");
    let inst_text = inst.to_string_lossy().into_owned();
    let dev = scratch.path().join("git").to_string_lossy().into_owned();
    // Latest-dev under latest: trusted checkout, no installer line.
    provider_case(&ProviderScenario {
        home: home_text.clone(),
        provider: "shdeps".to_string(),
        provider_set: true,
        policy: "latest".to_string(),
        conf_ok: true,
        dev_dir: dev.clone(),
        dev_exists: true,
        dev_valid: true,
        inst_ok: true,
        inst_path: inst_text.clone(),
        inst_source: "latest-dev".to_string(),
        dev_rev_ok: true,
        binary: "x".to_string(),
        binary_set: true,
        abi_expected: "7".to_string(),
        abi_expected_ok: true,
        abi_actual: "abi:7".to_string(),
        abi_ok: true,
        shdepsw_bin: bin_text.clone(),
        ..Default::default()
    });
    // Managed under latest with an invalid checkout warns first.
    provider_case(&ProviderScenario {
        home: home_text.clone(),
        provider: "shdeps".to_string(),
        provider_set: true,
        policy: "latest".to_string(),
        conf_ok: true,
        dev_dir: dev.clone(),
        dev_exists: true,
        inst_ok: true,
        inst_path: inst_text.clone(),
        inst_source: "managed".to_string(),
        binary: "x".to_string(),
        binary_set: true,
        abi_expected: "7".to_string(),
        abi_expected_ok: true,
        abi_actual: "abi:7".to_string(),
        abi_ok: true,
        shdepsw_bin: bin_text.clone(),
        ..Default::default()
    });
    // Unknown source fails the selection.
    provider_case(&ProviderScenario {
        home: home_text,
        provider: "shdeps".to_string(),
        provider_set: true,
        conf_ok: true,
        dev_dir: dev,
        inst_ok: true,
        inst_path: inst_text,
        inst_source: "stale-cache".to_string(),
        shdepsw_bin: bin_text,
        ..Default::default()
    });
}

/// Missing binary, missing ABI expectation, and failed ABI probe.
#[test]
fn provider_binary_abi_failures_matches() {
    let scratch = TempDir::new("doctor-prov-abi").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let home_text = home.to_string_lossy().into_owned();
    let inst = home.join("install.sh");
    let inst_text = inst.to_string_lossy().into_owned();
    let dev = scratch.path().join("git").to_string_lossy().into_owned();
    // Binary unavailable ends the check (no preselect, no
    // installer sibling yet).
    provider_case(&ProviderScenario {
        home: home_text.clone(),
        provider: "shdeps".to_string(),
        provider_set: true,
        conf_ok: true,
        dev_dir: dev.clone(),
        inst_ok: true,
        inst_path: inst_text.clone(),
        inst_source: "managed".to_string(),
        ..Default::default()
    });
    // A real binary fixture: the shell `_dr_shdeps_binary`
    // resolves for real, so ABI scenarios preselect it.
    let bin = home.join("shdeps");
    exec_fixture(&bin);
    let bin_text = bin.to_string_lossy().into_owned();
    // No lock ABI: expected reads `<missing>`.
    provider_case(&ProviderScenario {
        home: home_text.clone(),
        provider: "shdeps".to_string(),
        provider_set: true,
        conf_ok: true,
        dev_dir: dev.clone(),
        inst_ok: true,
        inst_path: inst_text.clone(),
        inst_source: "managed".to_string(),
        binary: "x".to_string(),
        binary_set: true,
        abi_actual: "abi:7".to_string(),
        abi_ok: true,
        shdepsw_bin: bin_text.clone(),
        ..Default::default()
    });
    // Failed ABI probe: found reads `<unavailable>`.
    provider_case(&ProviderScenario {
        home: home_text,
        provider: "shdeps".to_string(),
        provider_set: true,
        conf_ok: true,
        dev_dir: dev,
        inst_ok: true,
        inst_path: inst_text,
        inst_source: "managed".to_string(),
        binary: "x".to_string(),
        binary_set: true,
        abi_expected: "7".to_string(),
        abi_expected_ok: true,
        shdepsw_bin: bin_text,
        ..Default::default()
    });
}

/// Identity oracle: real `dot_xdg_path` plus the check module.
/// Witness: `rc=<status>`.
const IDENTITY_PRELUDE: &str = concat!(
    "set -u\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/doctor/repos.sh\"\n",
);

fn identity_case(state: Option<&Path>, home: &Path, marker_body: Option<&[u8]>, plant_link: bool) {
    let home_text = home.to_string_lossy().into_owned();
    if let (Some(state), Some(body)) = (state, marker_body) {
        let marker = state.join("dot").join("init").join("completed");
        std::fs::create_dir_all(marker.parent().expect("parent")).expect("marker dir");
        if plant_link {
            let target = state.join("dot").join("init").join("real-target");
            std::fs::write(&target, body).expect("link target");
            std::os::unix::fs::symlink(&target, &marker).expect("plant marker symlink");
        } else {
            std::fs::write(&marker, body).expect("marker");
        }
    }
    let mut set = vec![("HOME", home_text.as_str())];
    let state_text;
    if let Some(state) = state {
        state_text = state.to_string_lossy().into_owned();
        set.push(("XDG_STATE_HOME", state_text.as_str()));
    }
    let remove = if state.is_none() {
        vec!["XDG_STATE_HOME"]
    } else {
        Vec::new()
    };
    let (code, stdout, _) = shell_oracle(
        &set,
        &remove,
        &format!(
            "{IDENTITY_PRELUDE}_dr_completed_identity_matches_home; printf 'rc=%d\\n' \"$?\"\n"
        ),
    );
    assert_eq!(code, 0, "oracle failed");
    let marker = state.map(|state| state.join("dot").join("init").join("completed"));
    let rust = completed_identity_matches_home(marker.as_deref(), &home_text);
    let witnessed = format!("rc={}\n", i32::from(!rust));
    assert_eq!(witnessed, stdout, "identity diverged");
}

/// Good marker: worktree plus git dir both name HOME.
#[test]
fn identity_good_matches() {
    let scratch = TempDir::new("doctor-ident-good").expect("scratch");
    let state = scratch.path().join("state");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let body = format!(
        "git_dir={}/.git\nworktree={}\n",
        home.to_string_lossy(),
        home.to_string_lossy()
    );
    identity_case(Some(&state), &home, Some(body.as_bytes()), false);
}

/// Marker matrix: unresolvable, missing, symlink, wrong
/// worktree, wrong git dir, last-wins ordering.
#[test]
fn identity_matrix_matches() {
    let scratch = TempDir::new("doctor-ident-matrix").expect("scratch");
    // Unresolvable xdg path (no HOME, no XDG roots at all).
    let (code, stdout, _) = shell_oracle(
        &[],
        &[
            "HOME",
            "XDG_STATE_HOME",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
        ],
        &format!(
            "{IDENTITY_PRELUDE}_dr_completed_identity_matches_home; printf 'rc=%d\\n' \"$?\"\n"
        ),
    );
    assert_eq!(code, 0, "oracle failed");
    assert_eq!(stdout, "rc=1\n", "unresolvable oracle: {stdout:?}");
    assert!(!completed_identity_matches_home(None, "/nonexistent"));

    let state = scratch.path().join("state");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let home_text = home.to_string_lossy().into_owned();
    // Missing marker file.
    identity_case(Some(&state), &home, None, false);
    // Symlink marker is rejected.
    let good = format!("git_dir={home_text}/.git\nworktree={home_text}\n");
    identity_case(Some(&state), &home, Some(good.as_bytes()), true);
    // Wrong worktree.
    let bad_tree = format!("git_dir={home_text}/.git\nworktree=/elsewhere\n");
    identity_case(Some(&state), &home, Some(bad_tree.as_bytes()), false);
    // Wrong git dir.
    let bad_git = format!("git_dir=/elsewhere/.git\nworktree={home_text}\n");
    identity_case(Some(&state), &home, Some(bad_git.as_bytes()), false);
    // Last occurrence wins.
    let last_wins = format!("worktree=/elsewhere\ngit_dir=/elsewhere/.git\n{good}");
    identity_case(Some(&state), &home, Some(last_wins.as_bytes()), false);
}

/// Client-checkout oracle: real git plus the check module.
const CLIENT_PRELUDE: &str = concat!(
    "set -u\n",
    ". \"$1/lib/dot/public/xdg.sh\"\n",
    ". \"$1/lib/dot/doctor/repos.sh\"\n",
);

fn client_checkout_case(home: &Path, state: &Path, marker_body: Option<&[u8]>) {
    if let Some(body) = marker_body {
        let marker = state.join("dot").join("init").join("completed");
        std::fs::create_dir_all(marker.parent().expect("parent")).expect("marker dir");
        std::fs::write(&marker, body).expect("marker");
    }
    let home_text = home.to_string_lossy().into_owned();
    let state_text = state.to_string_lossy().into_owned();
    let (code, stdout, _) = shell_oracle(
        &[
            ("HOME", home_text.as_str()),
            ("XDG_STATE_HOME", state_text.as_str()),
        ],
        &[],
        &format!("{CLIENT_PRELUDE}_dr_is_client_checkout; printf 'rc=%d\\n' \"$?\"\n"),
    );
    assert_eq!(code, 0, "oracle failed");
    let marker = state.join("dot").join("init").join("completed");
    let rust = is_client_checkout(home, Some(&marker));
    let witnessed = format!("rc={}\n", i32::from(!rust));
    assert_eq!(witnessed, stdout, "client checkout diverged");
}

/// Not a repository at all.
#[test]
fn client_checkout_not_a_repo_matches() {
    let scratch = TempDir::new("doctor-client-norepo").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let state = scratch.path().join("state");
    client_checkout_case(&home, &state, None);
}

/// Repository rooted at HOME: identity marker, config flag,
/// neither, and a HOME nested inside a larger checkout.
#[test]
fn client_checkout_matrix_matches() {
    let scratch = TempDir::new("doctor-client-matrix").expect("scratch");
    // Rooted repo with a good marker.
    let home = scratch.path().join("home");
    git_init_seeded(&home);
    let state = scratch.path().join("state");
    let home_text = home.to_string_lossy().into_owned();
    let good = format!("git_dir={home_text}/.git\nworktree={home_text}\n");
    client_checkout_case(&home, &state, Some(good.as_bytes()));
    // Bad marker but the config flag saves it.
    let state2 = scratch.path().join("state2");
    git(&home, &["config", "dot.clientRepository", "true"]);
    let bad = "git_dir=/elsewhere/.git\nworktree=/elsewhere\n".to_string();
    client_checkout_case(&home, &state2, Some(bad.as_bytes()));
    // Bad marker and no flag.
    let home3 = scratch.path().join("home3");
    git_init_seeded(&home3);
    let state3 = scratch.path().join("state3");
    client_checkout_case(&home3, &state3, Some(bad.as_bytes()));
    // HOME nested inside a checkout is not the checkout root.
    let outer = scratch.path().join("outer");
    git_init_seeded(&outer);
    let nested = outer.join("sub");
    std::fs::create_dir_all(&nested).expect("nested");
    let state4 = scratch.path().join("state4");
    client_checkout_case(&nested, &state4, None);
}

/// Base-repo oracle prelude: real runtime/paths/check modules
/// plus a `_base_git` dispatch copied from `repos/model.sh` and
/// stubbed existence/identity verdicts.
const BASE_PRELUDE: &str = concat!(
    "set -u\n",
    ". \"$1/lib/dot/doctor/runtime.sh\"\n",
    ". \"$1/lib/dot/doctor/paths.sh\"\n",
    ". \"$1/lib/dot/doctor/repos.sh\"\n",
    "_base_repo_exists() { [[ $DOT_BASE_TOPOLOGY != missing ]]; }\n",
    "_base_git() { case $DOT_BASE_TOPOLOGY in separate) command git --git-dir=\"$DOT_CLIENT_GIT_DIR\" --work-tree=\"$HOME\" \"$@\";; ordinary) command git -C \"$HOME\" \"$@\";; *) return 128;; esac; }\n",
    "_dr_is_client_checkout() { [[ ${IS_CLIENT:-0} == 1 ]]; }\n",
);

/// One base-repo scenario over real git fixtures.
#[allow(clippy::too_many_arguments)]
fn base_repo_case(topology: &str, client_git_dir: &Path, home: &Path, is_client: bool) {
    let home_text = home.to_string_lossy().into_owned();
    let git_dir_text = client_git_dir.to_string_lossy().into_owned();
    let (code, stdout, _) = shell_oracle(
        &[
            ("DOT_BASE_TOPOLOGY", topology),
            ("DOT_CLIENT_GIT_DIR", git_dir_text.as_str()),
            ("HOME", home_text.as_str()),
            ("IS_CLIENT", if is_client { "1" } else { "0" }),
        ],
        &[],
        &format!("{BASE_PRELUDE}_dr_check_base_repo\n"),
    );
    assert_eq!(code, 0, "oracle failed");
    let rust = check_base_repo(&BaseRepoInputs {
        topology,
        client_git_dir: git_dir_text.as_str(),
        home: home_text.as_str(),
        is_client_checkout: is_client,
    });
    assert_eq!(render(&rust), stdout, "base repo diverged ({topology})");
}

/// Missing repository: client checkout or not.
#[test]
fn base_repo_missing_matches() {
    let scratch = TempDir::new("doctor-base-missing").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let git_dir = scratch.path().join("gitdir");
    base_repo_case("missing", &git_dir, &home, true);
    base_repo_case("missing", &git_dir, &home, false);
}

/// Ordinary checkout, clean and current.
#[test]
fn base_repo_ordinary_clean_matches() {
    let scratch = TempDir::new("doctor-base-ord").expect("scratch");
    let origin = scratch.path().join("origin");
    git_init_seeded(&origin);
    let home = scratch.path().join("home");
    let status = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(&origin)
        .arg(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone");
    assert!(status.success());
    let git_dir = scratch.path().join("gitdir");
    base_repo_case("ordinary", &git_dir, &home, false);
}

/// Ordinary checkout with tracked dirt (untracked files excluded
/// from the count) and a detached HEAD.
#[test]
fn base_repo_ordinary_dirty_detached_matches() {
    let scratch = TempDir::new("doctor-base-dirty").expect("scratch");
    let origin = scratch.path().join("origin");
    git_init_seeded(&origin);
    let home = scratch.path().join("home");
    let status = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(&origin)
        .arg(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone");
    assert!(status.success());
    std::fs::write(home.join("tracked.txt"), "v1\n").expect("tracked");
    git(&home, &["add", "tracked.txt"]);
    git(&home, &["commit", "-q", "-m", "add tracked"]);
    std::fs::write(home.join("tracked.txt"), "v2\n").expect("dirty");
    std::fs::write(home.join("untracked.txt"), "u\n").expect("untracked");
    git(&home, &["checkout", "-q", "--detach", "HEAD"]);
    let git_dir = scratch.path().join("gitdir");
    base_repo_case("ordinary", &git_dir, &home, false);
}

/// Ordinary checkout with no upstream configured.
#[test]
fn base_repo_ordinary_no_upstream_matches() {
    let scratch = TempDir::new("doctor-base-noup").expect("scratch");
    let home = scratch.path().join("home");
    git_init_seeded(&home);
    let git_dir = scratch.path().join("gitdir");
    base_repo_case("ordinary", &git_dir, &home, false);
}

/// Ordinary upstream distance: behind, ahead, diverged.
#[test]
fn base_repo_ordinary_upstream_distance_matches() {
    // Behind: origin gains a commit after the clone.
    let scratch = TempDir::new("doctor-base-dist").expect("scratch");
    let origin = scratch.path().join("origin");
    git_init_seeded(&origin);
    let home = scratch.path().join("home");
    let status = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(&origin)
        .arg(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone");
    assert!(status.success());
    git(
        &origin,
        &["commit", "-q", "--allow-empty", "-m", "origin-ahead"],
    );
    git(&home, &["fetch", "-q", "origin"]);
    let git_dir = scratch.path().join("gitdir");
    base_repo_case("ordinary", &git_dir, &home, false);
    // Ahead: client commits without pushing.
    git(
        &home,
        &["commit", "-q", "--allow-empty", "-m", "client-ahead"],
    );
    base_repo_case("ordinary", &git_dir, &home, false);
    // Diverged: origin moves again after the client commit.
    git(
        &origin,
        &["commit", "-q", "--allow-empty", "-m", "origin-ahead-2"],
    );
    git(&home, &["fetch", "-q", "origin"]);
    base_repo_case("ordinary", &git_dir, &home, false);
}

/// Ordinary checkout whose worktree is not HOME.
#[test]
fn base_repo_ordinary_mismatch_matches() {
    let scratch = TempDir::new("doctor-base-mismatch").expect("scratch");
    let outer = scratch.path().join("outer");
    git_init_seeded(&outer);
    let nested = outer.join("sub");
    std::fs::create_dir_all(&nested).expect("nested");
    let git_dir = scratch.path().join("gitdir");
    base_repo_case("ordinary", &git_dir, &nested, false);
}

/// Separate topology over a bare git dir: legacy layout.
#[test]
fn base_repo_separate_bare_matches() {
    let scratch = TempDir::new("doctor-base-bare").expect("scratch");
    let git_dir = scratch.path().join("client.git");
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg("--bare")
        .arg(&git_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("init bare");
    assert!(status.success());
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    base_repo_case("separate", &git_dir, &home, false);
}

/// Separate topology with an explicit worktree: tilde display.
#[test]
fn base_repo_separate_worktree_matches() {
    let scratch = TempDir::new("doctor-base-worktree").expect("scratch");
    let home = scratch.path().join("home");
    let git_dir = scratch.path().join("client.git");
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg("--separate-git-dir")
        .arg(&git_dir)
        .arg(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("init separate");
    assert!(status.success());
    git(&home, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    base_repo_case("separate", &git_dir, &home, false);
}

/// Separate topology with neither bare nor worktree identity.
#[test]
fn base_repo_separate_no_identity_matches() {
    let scratch = TempDir::new("doctor-base-noid").expect("scratch");
    let git_dir = scratch.path().join("client.git");
    let status = Command::new("git")
        .arg("init")
        .arg("-q")
        .arg("--bare")
        .arg(&git_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("init bare");
    assert!(status.success());
    git(&git_dir, &["config", "core.bare", "false"]);
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    base_repo_case("separate", &git_dir, &home, false);
}

/// Unrecognized topology: existing with every git call failing.
#[test]
fn base_repo_unrecognized_topology_matches() {
    let scratch = TempDir::new("doctor-base-weird").expect("scratch");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let git_dir = scratch.path().join("gitdir");
    base_repo_case("liminal", &git_dir, &home, false);
}

/// Upstream resolves but cannot be compared (remote ref points
/// at a non-commit object).
#[test]
fn base_repo_uncomparable_upstream_matches() {
    let scratch = TempDir::new("doctor-base-uncomp").expect("scratch");
    let origin = scratch.path().join("origin");
    git_init_seeded(&origin);
    let home = scratch.path().join("home");
    let status = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(&origin)
        .arg(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("clone");
    assert!(status.success());
    git(&home, &["fetch", "-q", "origin"]);
    let blob = Command::new("git")
        .arg("-C")
        .arg(&home)
        .arg("hash-object")
        .arg("-w")
        .arg("--stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(b"not a commit\n")?;
            child.wait_with_output()
        })
        .expect("hash-object");
    assert!(blob.status.success());
    let sha = String::from_utf8_lossy(&blob.stdout)
        .trim_end_matches('\n')
        .to_string();
    git(&home, &["update-ref", "refs/remotes/origin/main", &sha]);
    let git_dir = scratch.path().join("gitdir");
    base_repo_case("ordinary", &git_dir, &home, false);
}
