//! Contracts for the hook runtime's conditional file mutation
//! (`lib/dot/public/hook-runtime-v1/temp.sh`): generation tokens that bind
//! a destination's physical parent and content, and the journaled
//! prepare/quarantine/commit transaction that makes every crash window
//! recoverable.
//!
//! Crash windows are modeled by running the internal phases a public helper
//! composes (`_dot_file_transaction_prepare`, `_quarantine`, `_recover`) and
//! stopping between them, then letting the next public generation capture
//! recover.

#[path = "support/hook_runtime.rs"]
mod hook_runtime;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use dot_test_support::TempDir;
use hook_runtime::Runtime;

fn file(root: &Path, name: &str, bytes: &[u8], mode: u32) -> PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// Run `script` against the runtime with `args`, requiring success; returns
/// stdout as text.
fn run<I, S>(home: &Path, script: &str, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    String::from_utf8(Runtime::new(home, script).args(args).stdout()).expect("utf8 stdout")
}

/// The transaction directory the runtime journals `destination` under.
fn transaction_dir(destination: &Path) -> PathBuf {
    let parent = destination.parent().unwrap().canonicalize().unwrap();
    let name = destination.file_name().unwrap().to_str().unwrap();
    parent.join(format!(".{name}.dot-file-transaction-v1"))
}

fn field<'a>(stdout: &'a str, key: &str) -> &'a str {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("missing {key} in {stdout:?}"))
}

#[test]
fn target_generation_and_signature_bind_path_parent_and_content() {
    let dir = TempDir::new("generation-binding").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"v1\n", 0o640);
    let absent = dir.path().join("home/absent");
    let stdout = run(
        dir.path(),
        r#"
_dot_file_target_resolve "$1"
printf 'path=%s\nparent=%s\ntransaction=%s\n' \
  "$DOT_FILE_TARGET_PATH" "$DOT_FILE_TARGET_PARENT" "$DOT_FILE_TARGET_TRANSACTION"
printf 'target-digest=%s\ntarget-parent=%s\n' \
  "$DOT_FILE_TARGET_PATH_DIGEST" "$DOT_FILE_TARGET_PARENT_ID"
printf 'signature=%s\n' "$(_dot_file_signature "$1")"
token=$(_dot_file_generation_raw "$1")
_dot_file_generation_validate "$token"
printf 'state=%s\ndigest=%s\nparent-id=%s\nbound-signature=%s\n' \
  "$DOT_FILE_GENERATION_STATE" "$DOT_FILE_GENERATION_PATH_DIGEST" \
  "$DOT_FILE_GENERATION_PARENT_ID" "$DOT_FILE_GENERATION_SIGNATURE"
_dot_file_generation_validate "$(_dot_file_generation_raw "$2")"
printf 'absent=%s|%s\n' "$DOT_FILE_GENERATION_STATE" "$DOT_FILE_GENERATION_SIGNATURE"
if [[ ${token: -1} == 0 ]]; then flip=1; else flip=0; fi
for bad in '' 'v2|bad' "$token"$'\n' "${token%?}$flip"; do
  printf 'bad=%s\n' "$(rc _dot_file_generation_validate "$bad")"
done
printf 'relative=%s\n' "$(rc _dot_file_target_resolve relative)"
printf 'newline=%s\n' "$(rc _dot_file_target_resolve "$3")"
"#,
        [
            dst.as_os_str(),
            absent.as_os_str(),
            dir.path().join("bad\nname").as_os_str(),
        ],
    );
    let canonical = dst.canonicalize().unwrap();
    assert_eq!(field(&stdout, "path"), canonical.to_str().unwrap());
    assert_eq!(
        field(&stdout, "parent"),
        canonical.parent().unwrap().to_str().unwrap()
    );
    assert_eq!(
        field(&stdout, "transaction"),
        transaction_dir(&dst).to_str().unwrap()
    );
    let signature: Vec<&str> = field(&stdout, "signature").split('|').collect();
    let meta = std::fs::metadata(&dst).unwrap();
    assert_eq!(signature[0], meta.dev().to_string());
    assert_eq!(signature[1], meta.ino().to_string());
    assert_eq!(signature[2], "640");
    assert_eq!(signature[3], "3");
    assert!(matches!(signature[4].len(), 40 | 64), "{signature:?}");
    assert_eq!(field(&stdout, "state"), "file");
    assert_eq!(field(&stdout, "digest"), field(&stdout, "target-digest"));
    assert_eq!(field(&stdout, "parent-id"), field(&stdout, "target-parent"));
    assert_eq!(
        field(&stdout, "bound-signature"),
        field(&stdout, "signature")
    );
    assert_eq!(field(&stdout, "absent"), "absent|-|-|-|-|-");
    let bad: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("bad="))
        .collect();
    assert_eq!(bad, ["1", "1", "1", "1"]);
    assert_eq!(field(&stdout, "relative"), "1");
    assert_eq!(field(&stdout, "newline"), "1");
}

#[test]
fn replace_and_remove_lifecycle_commit_only_matching_generation() {
    let dir = TempDir::new("generation-lifecycle").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"v1\n", 0o644);
    let source = dir.path().join("home/app.conf.new");
    let stdout = run(
        dir.path(),
        r#"
token=$(dot_file_generation "$1")
printf 'v2\n' >"$2"
printf 'commit=%s\n' "$(rc dot_commit_tmp_if_generation "$2" "$1" "$token")"
printf 'after-commit=%s\n' "$(cat "$1")"
printf 'source-gone=%s\n' "$([[ -e $2 ]] && echo no || echo yes)"
printf 'v3\n' >"$2"
printf 'stale=%s\n' "$(rc dot_commit_tmp_if_generation "$2" "$1" "$token")"
printf 'after-stale=%s\n' "$(cat "$1")"
printf 'remove=%s\n' "$(rc dot_remove_if_generation "$1" "$(dot_file_generation "$1")")"
printf 'removed=%s\n' "$([[ -e $1 ]] && echo no || echo yes)"
printf 'usage=%s,%s,%s\n' "$(rc dot_file_generation)" \
  "$(rc dot_commit_tmp_if_generation "$2" "$1")" "$(rc dot_remove_if_generation "$1")"
"#,
        [&dst, &source],
    );
    assert_eq!(
        stdout,
        "commit=0\nafter-commit=v2\nsource-gone=yes\nstale=1\nafter-stale=v2\n\
         remove=0\nremoved=yes\nusage=2,2,2\n"
    );
    assert!(!transaction_dir(&dst).exists());
}

#[test]
fn remove_refuses_an_absent_generation_after_late_creation() {
    let dir = TempDir::new("generation-remove-late-create").unwrap();
    let dst = dir.path().join("home/app.conf");
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    let stdout = run(
        dir.path(),
        r#"
absent=$(dot_file_generation "$1")
printf 'late creation\n' >"$1"
rc dot_remove_if_generation "$1" "$absent"
"#,
        [&dst],
    );
    assert_eq!(stdout, "1\n");
    assert_eq!(std::fs::read(&dst).unwrap(), b"late creation\n");
}

#[test]
fn malformed_generation_cannot_remove_live_content() {
    let dir = TempDir::new("generation-remove-malformed").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"live\n", 0o644);
    let stdout = run(
        dir.path(),
        "rc dot_remove_if_generation \"$1\" 'v1|forged'\n",
        [&dst],
    );
    assert_eq!(stdout, "1\n");
    assert_eq!(std::fs::read(&dst).unwrap(), b"live\n");
}

#[test]
fn generation_capture_rejects_a_symlink_destination() {
    let dir = TempDir::new("generation-symlink").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    let link = dir.path().join("home/link");
    std::os::unix::fs::symlink(&dst, &link).unwrap();
    assert_eq!(
        run(dir.path(), "rc dot_file_generation \"$1\"\n", [&link]),
        "1\n"
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"managed\n");
}

#[test]
fn generation_capture_requires_an_outer_update_lock() {
    let dir = TempDir::new("generation-lock").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    // Either the test gate or an update-lock token admits a capture.
    for (test_mode, token, expected) in [
        (None, None, "1\n"),
        (Some("1"), None, "0\n"),
        (None, Some("token"), "0\n"),
        (Some("1"), Some("token"), "0\n"),
        (Some("0"), None, "1\n"),
        (None, Some(""), "1\n"),
    ] {
        let mut runtime = Runtime::new(
            dir.path(),
            "if dot_file_generation \"$1\" >/dev/null; then echo 0; else echo \"$?\"; fi\n",
        )
        .arg(&dst)
        .env_remove("DOT_TEST");
        if let Some(value) = test_mode {
            runtime = runtime.env("DOT_TEST", value);
        }
        if let Some(value) = token {
            runtime = runtime.env("DOT_UPDATE_LOCK_TOKEN", value);
        }
        assert_eq!(
            String::from_utf8(runtime.stdout()).unwrap(),
            expected,
            "DOT_TEST={test_mode:?} DOT_UPDATE_LOCK_TOKEN={token:?}"
        );
    }
    assert_eq!(std::fs::read(&dst).unwrap(), b"managed\n");
}

#[test]
fn generation_token_cannot_follow_a_replaced_parent_symlink() {
    let dir = TempDir::new("generation-parent-swap").unwrap();
    let parent_a = dir.path().join("parent-a");
    let parent_b = dir.path().join("parent-b");
    let selected = dir.path().join("selected-parent");
    let first = file(&parent_a, "config", b"parent a\n", 0o644);
    let second = file(&parent_b, "config", b"parent b\n", 0o644);
    std::os::unix::fs::symlink(&parent_a, &selected).unwrap();
    let logical = selected.join("config");
    let candidate = parent_b.join("candidate");
    let stdout = run(
        dir.path(),
        r#"
generation=$(dot_file_generation "$1")
ln -sfn "$3" "$2"
printf 'redirected update\n' >"$4"
rc dot_commit_tmp_if_generation "$4" "$1" "$generation"
"#,
        [&logical, &selected, &parent_b, &candidate],
    );
    assert_eq!(stdout, "1\n");
    assert_eq!(std::fs::read(first).unwrap(), b"parent a\n");
    assert_eq!(std::fs::read(second).unwrap(), b"parent b\n");
    assert_eq!(std::fs::read(candidate).unwrap(), b"redirected update\n");
}

/// Prepare a replacement of `$1` by `$2` and quarantine the live file —
/// the state a crash between quarantine and publication leaves behind.
const PREPARE_AND_QUARANTINE: &str = r#"
token=$(_dot_file_generation_raw "$1")
_dot_file_transaction_prepare replace "$2" "$1" "$token"
DOT_FILE_TRANSACTION_OPERATION=replace
transaction=$DOT_FILE_TRANSACTION_PATH
_dot_file_transaction_quarantine
"#;

#[test]
fn publication_conflict_preserves_the_late_winner_and_cleans_the_journal() {
    let dir = TempDir::new("generation-publish-conflict").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    let source = file(dir.path(), "home/candidate", b"candidate\n", 0o600);
    let stdout = run(
        dir.path(),
        &format!(
            "{PREPARE_AND_QUARANTINE}
printf 'late winner\\n' >\"$1\"
printf 'publish=%s\\n' \"$(rc _dot_move_noreplace \"$transaction/candidate\" \"$1\")\"
printf 'recover=%s\\n' \"$(rc _dot_file_transaction_recover \"$1\" \"$transaction\")\"
"
        ),
        [&dst, &source],
    );
    assert_eq!(stdout, "publish=1\nrecover=0\n");
    assert_eq!(std::fs::read(&dst).unwrap(), b"late winner\n");
    assert!(!transaction_dir(&dst).exists());
}

#[test]
fn quarantine_conflict_preserves_the_replacement_and_cleans_the_journal() {
    let dir = TempDir::new("generation-quarantine-conflict").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    let source = file(dir.path(), "home/candidate", b"candidate\n", 0o600);
    // Model the winner arriving after quarantine's generation recheck but
    // immediately before its no-replace move. Recovery must identify the
    // moved file as foreign by signature and restore it to the live name.
    let stdout = run(
        dir.path(),
        r#"
token=$(_dot_file_generation_raw "$1")
_dot_file_transaction_prepare replace "$2" "$1" "$token"
transaction=$DOT_FILE_TRANSACTION_PATH
printf 'quarantine winner\n' >"$1"
_dot_move_noreplace "$1" "$transaction/previous"
rc _dot_file_transaction_recover "$1" "$transaction"
"#,
        [&dst, &source],
    );
    assert_eq!(stdout, "0\n");
    assert_eq!(std::fs::read(&dst).unwrap(), b"quarantine winner\n");
    assert!(!source.exists());
    assert!(!transaction_dir(&dst).exists());
}

/// Leave a replacement journal at the selected crash boundary, then trigger
/// recovery through the next public generation capture.
fn recover_replace_after_crash(publish: bool, expected: &[u8]) {
    let dir = TempDir::new("generation-replace-crash").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"before crash\n", 0o644);
    let source = file(dir.path(), "home/candidate", b"after crash\n", 0o600);
    let publish = if publish {
        "_dot_move_noreplace \"$transaction/candidate\" \"$1\""
    } else {
        ":"
    };
    run(
        dir.path(),
        &format!("{PREPARE_AND_QUARANTINE}\n{publish}\ndot_file_generation \"$1\" >/dev/null\n"),
        [&dst, &source],
    );
    assert_eq!(std::fs::read(&dst).unwrap(), expected);
    assert!(!transaction_dir(&dst).exists());
}

#[test]
fn replace_recovery_restores_previous_content_after_prepublication_crash() {
    recover_replace_after_crash(false, b"before crash\n");
}

#[test]
fn replace_recovery_keeps_candidate_after_postpublication_crash() {
    recover_replace_after_crash(true, b"after crash\n");
}

/// Leave a removal journal at the selected crash boundary, optionally create
/// a late live winner, then trigger recovery through generation capture.
fn recover_remove_after_crash(committed: bool, late: Option<&str>, expected: Option<&[u8]>) {
    let dir = TempDir::new("generation-remove-crash").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    let commit = if committed {
        "_dot_file_transaction_record_write \"$transaction\" remove committed \"$token\" -"
    } else {
        ":"
    };
    let late = late.map_or(String::from(":"), |bytes| {
        format!("printf '%s' '{bytes}' >\"$1\"")
    });
    run(
        dir.path(),
        &format!(
            r#"
token=$(_dot_file_generation_raw "$1")
_dot_file_transaction_prepare remove '' "$1" "$token"
DOT_FILE_TRANSACTION_OPERATION=remove
transaction=$DOT_FILE_TRANSACTION_PATH
_dot_file_transaction_quarantine
{commit}
{late}
dot_file_generation "$1" >/dev/null
"#
        ),
        [&dst],
    );
    assert_eq!(std::fs::read(&dst).ok().as_deref(), expected);
    assert!(!transaction_dir(&dst).exists());
}

#[test]
fn remove_recovery_restores_file_after_precommit_crash() {
    recover_remove_after_crash(false, None, Some(b"managed\n"));
}

#[test]
fn remove_recovery_keeps_file_absent_after_postcommit_crash() {
    recover_remove_after_crash(true, None, None);
}

#[test]
fn remove_recovery_preserves_a_late_creation() {
    recover_remove_after_crash(false, Some("late winner"), Some(b"late winner"));
}

#[test]
fn committed_content_survives_cleanup_failure_and_next_capture_retries_cleanup() {
    let dir = TempDir::new("generation-cleanup-retry").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
    let source = file(dir.path(), "home/candidate", b"cleanup retry\n", 0o600);
    let transaction = transaction_dir(&dst);
    let stdout = run(
        dir.path(),
        &format!(
            "{PREPARE_AND_QUARANTINE}
_dot_move_noreplace \"$transaction/candidate\" \"$1\"
_dot_file_transaction_record_write \"$transaction\" replace committed \"$token\" \\
  \"$DOT_FILE_TRANSACTION_CANDIDATE\"
printf 'invalid transaction entry\\n' >\"$transaction/unexpected\"
printf 'cleanup=%s\\n' \"$(rc _dot_file_transaction_cleanup \"$transaction\")\"
printf 'retained=%s\\n' \"$([[ -d $transaction ]] && echo yes || echo no)\"
printf 'live=%s\\n' \"$(cat \"$1\")\"
rm -f -- \"$transaction/unexpected\"
dot_file_generation \"$1\" >/dev/null
"
        ),
        [&dst, &source],
    );
    assert_eq!(stdout, "cleanup=1\nretained=yes\nlive=cleanup retry\n");
    assert_eq!(std::fs::read(&dst).unwrap(), b"cleanup retry\n");
    assert!(!transaction.exists());
}

#[test]
fn generation_rejects_unmarked_or_nonprivate_transaction_directories_in_place() {
    for (label, mode) in [("unmarked", 0o700), ("nonprivate", 0o755)] {
        let dir = TempDir::new("generation-unsafe-transaction").unwrap();
        let dst = file(dir.path(), "home/app.conf", b"managed\n", 0o644);
        let transaction = transaction_dir(&dst);
        std::fs::create_dir(&transaction).unwrap();
        std::fs::set_permissions(&transaction, std::fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            run(dir.path(), "rc dot_file_generation \"$1\"\n", [&dst]),
            "1\n",
            "{label}"
        );
        assert!(transaction.is_dir(), "{label}");
        assert_eq!(std::fs::read(&dst).unwrap(), b"managed\n", "{label}");
    }
}

#[test]
fn rejected_transaction_setup_preserves_inputs_without_debris() {
    let dir = TempDir::new("generation-setup-failure").unwrap();
    let parent = dir.path().join("home");
    let dst = file(&parent, "app.conf", b"managed\n", 0o644);
    // A candidate outside the destination's directory cannot be published
    // atomically, so preparation refuses before journaling anything.
    let source = file(dir.path(), "staging/candidate", b"candidate\n", 0o600);
    assert_eq!(
        run(
            dir.path(),
            "rc dot_commit_tmp_if_generation \"$2\" \"$1\" \"$(dot_file_generation \"$1\")\"\n",
            [&dst, &source],
        ),
        "1\n"
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"managed\n");
    assert_eq!(std::fs::read(&source).unwrap(), b"candidate\n");
    let entries: Vec<_> = std::fs::read_dir(&parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["app.conf"]);
    assert_eq!(
        std::fs::read_dir(source.parent().unwrap()).unwrap().count(),
        1
    );
}

#[test]
fn prepare_journal_and_recovery_restore_quarantined_file() {
    let dir = TempDir::new("generation-recover").unwrap();
    let dst = file(dir.path(), "home/app.conf", b"old\n", 0o644);
    let source = file(dir.path(), "home/app.conf.new", b"new\n", 0o600);
    let stdout = run(
        dir.path(),
        r#"
token=$(_dot_file_generation_raw "$1")
_dot_file_transaction_prepare replace "$2" "$1" "$token"
transaction=$DOT_FILE_TRANSACTION_PATH
_dot_file_transaction_record_read "$transaction"
printf 'prepared=%s|%s|%s\n' "$DOT_FILE_TRANSACTION_OPERATION" \
  "$DOT_FILE_TRANSACTION_PHASE" "$([[ $DOT_FILE_TRANSACTION_CANDIDATE == - ]] && echo none || echo some)"
_dot_file_transaction_quarantine
printf 'quarantined-live=%s\n' "$([[ -e $1 ]] && echo present || echo absent)"
_dot_file_transaction_record_read "$transaction"
printf 'phase=%s\n' "$DOT_FILE_TRANSACTION_PHASE"
rc _dot_file_transaction_recover "$1" "$transaction"
"#,
        [&dst, &source],
    );
    assert_eq!(
        stdout,
        "prepared=replace|prepared|some\nquarantined-live=absent\nphase=quarantined\n0\n"
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"old\n");
    assert!(!transaction_dir(&dst).exists());
}

#[test]
fn journal_reader_rejects_every_corrupt_control_shape() {
    let dir = TempDir::new("generation-record-bad").unwrap();
    let live = file(dir.path(), "app.conf", b"v1\n", 0o644);
    let token = run(dir.path(), "_dot_file_generation_raw \"$1\"\n", [&live]);
    let token = token.trim_end();
    for (label, body) in [
        ("bad-version", format!("v2\treplace\tprepared\t{token}\t-")),
        ("bad-op", format!("v1\trename\tprepared\t{token}\t-")),
        ("bad-phase", format!("v1\treplace\tstaged\t{token}\t-")),
        ("bad-token", "v1\treplace\tprepared\tbogus\t-".into()),
        (
            "bad-candidate",
            format!("v1\treplace\tprepared\t{token}\tbogus"),
        ),
        (
            "remove-candidate",
            format!(
                "v1\tremove\tprepared\t{token}\t1|2|644|3|{}",
                "0".repeat(40)
            ),
        ),
        ("four-fields", format!("v1\treplace\tprepared\t{token}")),
        ("empty", String::new()),
    ] {
        let transaction = dir.path().join(label);
        std::fs::create_dir(&transaction).unwrap();
        std::fs::set_permissions(&transaction, std::fs::Permissions::from_mode(0o700)).unwrap();
        let record = file(&transaction, "record", body.as_bytes(), 0o600);
        assert_eq!(
            run(
                dir.path(),
                "rc _dot_file_transaction_record_read \"$1\"\n",
                [&transaction]
            ),
            "1\n",
            "{label}"
        );
        assert!(record.exists(), "{label}");
    }
    // A stale `record.next` wedges the journal: the writer refuses rather
    // than clobber a half-written phase change.
    let transaction = dir.path().join("wedged");
    std::fs::create_dir(&transaction).unwrap();
    std::fs::set_permissions(&transaction, std::fs::Permissions::from_mode(0o700)).unwrap();
    file(&transaction, "record.next", b"stale\n", 0o600);
    assert_eq!(
        run(
            dir.path(),
            "rc _dot_file_transaction_record_write \"$1\" remove committed \"$2\" -\n",
            [transaction.as_os_str(), std::ffi::OsStr::new(token)],
        ),
        "1\n"
    );
}

#[test]
fn nonminimal_generation_validates_but_recovery_fails_closed() {
    let dir = TempDir::new("generation-forged").unwrap();
    let live = file(dir.path(), "home/app.conf", b"v1\n", 0o644);
    let transaction = transaction_dir(&live);
    // Re-seal a token whose parent device has a leading zero: well-formed
    // and correctly checksummed, but never equal to a live identity.
    let stdout = run(
        dir.path(),
        r#"
token=$(_dot_file_generation_raw "$1")
IFS='|' read -r -a fields <<<"$token"
fields[2]=007
payload=$(IFS='|'; printf '%s' "${fields[*]:0:10}")
forged="$payload|$(_dot_file_text_digest "dot-file-generation-v1|$payload")"
printf 'validate=%s\n' "$(rc _dot_file_generation_validate "$forged")"
_dot_file_target_resolve "$1"
mkdir -m 700 "$DOT_FILE_TARGET_TRANSACTION"
(umask 077; printf 'v1\treplace\tprepared\t%s\t1|2|644|3|%s\n' "$forged" \
  0000000000000000000000000000000000000000 >"$DOT_FILE_TARGET_TRANSACTION/record")
printf 'read=%s\n' "$(rc _dot_file_transaction_record_read "$DOT_FILE_TARGET_TRANSACTION")"
printf 'recover=%s\n' "$(rc _dot_file_transaction_recover "$DOT_FILE_TARGET_PATH" "$DOT_FILE_TARGET_TRANSACTION")"
"#,
        [&live],
    );
    assert_eq!(stdout, "validate=0\nread=0\nrecover=1\n");
    assert_eq!(std::fs::read(&live).unwrap(), b"v1\n");
    assert!(transaction.exists());
}
