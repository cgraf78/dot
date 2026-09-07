//! Native security contracts for one-use overlay authorization contexts.

use dot::overlay_context as context;
use dot_test_support::TempDir;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

fn euid() -> u32 {
    unsafe { libc::geteuid() }
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
fn private_dir(tag: &str) -> TempDir {
    let dir = TempDir::new(tag).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}
fn records(home: &str) -> Vec<Vec<u8>> {
    vec![format!("work|{home}/.dotfiles-work|https://example.test/work.git|{home}/.config/dot/overlays.d/10-work.conf|false|git").into_bytes()]
}
fn frame(
    token: &str,
    mode: &str,
    set_kind: &str,
    stage: &str,
    count: &str,
    records: &[Vec<u8>],
) -> Vec<u8> {
    let mut body = Vec::new();
    for field in [
        context::MAGIC,
        context::VERSION,
        token,
        mode,
        set_kind,
        stage,
        count,
    ] {
        body.extend_from_slice(field.as_bytes());
        body.push(0);
    }
    for record in records {
        for field in record.split(|byte| *byte == b'|') {
            body.extend_from_slice(field);
            body.push(0);
        }
    }
    body
}

#[test]
fn fields_paths_records_and_matrix_are_strict() {
    for (value, ok) in [
        (b"safe".as_slice(), true),
        (b"", true),
        (b"a|b", false),
        (b"a\nb", false),
        (b"a\tb", false),
        (&[127], false),
        (b"0123456789abcdef0123456789abcdef", false),
    ] {
        assert_eq!(context::field_safe(value), ok);
    }
    for (path, ok) in [
        (b"/a".as_slice(), true),
        (b"/a/b", true),
        (b"", false),
        (b"/", false),
        (b"a", false),
        (b"/a/", false),
        (b"/a//b", false),
        (b"/a/./b", false),
        (b"/a/../b", false),
        (b"/a\nb", false),
    ] {
        assert_eq!(context::absolute_canonical(path), ok);
    }
    let home = "/home/test";
    let valid = records(home).remove(0);
    assert!(context::record_validate(&valid, home));
    assert!(context::record_validate(
        b"local|/srv/local||/home/test/.config/dot/overlays.d/20-local.conf|false|none",
        home
    ));
    for bad in [
        b"work|wrong|url|/x/work.conf|false|git".as_slice(),
        b"dotfiles|/x|url|/x/dotfiles.conf|false|git",
        b"Work|/x|url|/x/Work.conf|false|git",
        b"work|/x||/x/work.conf|false|git",
        b"work|/x|url|/x/work.conf|maybe|git",
        b"work|/x||/x/work.conf|true|none",
        b"work|/x|url|/x/work.conf|false|other",
        b"work|/x|ur\x1bl|/x/work.conf|false|git",
        b"work|/x|ur\x7fl|/x/work.conf|false|git",
    ] {
        assert!(!context::record_validate(bad, home));
    }
    for (mode, set, stage) in [
        ("pre-sync", "eligible", "prepare"),
        ("pre-sync", "eligible", "reconcile"),
        ("merge", "active", "none"),
        ("deactivate", "retiring", "none"),
        ("doctor", "active", "none"),
    ] {
        assert!(context::matrix_valid(mode, set, stage));
    }
    assert!(!context::matrix_valid("merge", "eligible", "none"));
}

#[test]
fn directory_and_file_safety_enforce_mode_link_shape_and_freshness() {
    let dir = private_dir("context-safe");
    assert!(context::directory_safe(dir.path(), euid()));
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
    assert!(!context::directory_safe(dir.path(), euid()));
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = dir.path().join("context");
    std::fs::write(&file, b"x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(context::file_safe(&file, euid(), now()));
    assert!(!context::file_safe(&file, euid().wrapping_add(1), now()));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(!context::file_safe(&file, euid(), now()));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::hard_link(&file, dir.path().join("hardlink")).unwrap();
    assert!(!context::file_safe(&file, euid(), now()));
    assert!(!context::file_safe(&file, euid(), now() - 1_000));
    std::os::unix::fs::symlink(&file, dir.path().join("symlink")).unwrap();
    assert!(!context::file_safe(
        &dir.path().join("symlink"),
        euid(),
        now()
    ));
}

#[test]
fn tokens_are_random_lowercase_hex() {
    let a = context::token().unwrap();
    let b = context::token().unwrap();
    assert_ne!(a, b);
    for token in [a, b] {
        assert_eq!(token.len(), 64);
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
    }
}

#[test]
fn create_and_consume_are_single_use_and_preserve_context() {
    let dir = private_dir("context-roundtrip");
    let home = "/home/test";
    let (path, token) = context::create(
        dir.path(),
        "merge",
        "active",
        "none",
        &records(home),
        home,
        euid(),
        now(),
    )
    .unwrap();
    let decoded = context::consume(&path, &token, "merge", home, euid(), now()).unwrap();
    assert_eq!(
        decoded.records,
        records(home)
            .into_iter()
            .map(|v| String::from_utf8(v).unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(decoded.set_kind, "active");
    assert_eq!(decoded.stage, "none");
    assert!(
        !path.exists(),
        "authority is unlinked before decode returns"
    );
    assert!(context::consume(&path, &token, "merge", home, euid(), now()).is_err());
}

#[test]
fn consume_refuses_wrong_token_mode_permissions_links_and_frames() {
    let home = "/home/test";
    for mutation in ["token", "mode", "permissions", "hardlink", "frame"] {
        let dir = private_dir(&format!("context-{mutation}"));
        let (path, token) = context::create(
            dir.path(),
            "merge",
            "active",
            "none",
            &records(home),
            home,
            euid(),
            now(),
        )
        .unwrap();
        let mut presented = token.clone();
        let mut expected_mode = "merge";
        match mutation {
            "token" => {
                let replacement = if presented.starts_with('a') { "b" } else { "a" };
                presented.replace_range(..1, replacement);
            }
            "mode" => expected_mode = "doctor",
            "permissions" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap()
            }
            "hardlink" => std::fs::hard_link(&path, dir.path().join("second")).unwrap(),
            "frame" => std::fs::write(&path, b"bad\0frame\0").unwrap(),
            _ => unreachable!(),
        }
        assert!(
            context::consume(&path, &presented, expected_mode, home, euid(), now()).is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn decoder_rejects_each_malformed_frame_after_consuming_its_authority() {
    let home = "/home/test";
    for malformed in [
        "magic",
        "version",
        "token-shape",
        "kind",
        "stage",
        "duplicate",
        "count-mismatch",
        "leading-zero-count",
        "truncated",
        "trailing-nul",
        "trailing-bytes",
        "escape",
        "delete",
    ] {
        let dir = private_dir(&format!("context-frame-{malformed}"));
        let (path, token) = context::create(
            dir.path(),
            "pre-sync",
            "eligible",
            "prepare",
            &records(home),
            home,
            euid(),
            now(),
        )
        .unwrap();
        let mut presented = token.clone();
        let mut body = match malformed {
            "kind" => frame(&token, "pre-sync", "active", "prepare", "1", &records(home)),
            "stage" => frame(&token, "pre-sync", "eligible", "none", "1", &records(home)),
            "duplicate" => frame(
                &token,
                "pre-sync",
                "eligible",
                "prepare",
                "2",
                &[records(home)[0].clone(), records(home)[0].clone()],
            ),
            "count-mismatch" => frame(
                &token,
                "pre-sync",
                "eligible",
                "prepare",
                "2",
                &records(home),
            ),
            "leading-zero-count" => frame(
                &token,
                "pre-sync",
                "eligible",
                "prepare",
                "01",
                &records(home),
            ),
            "escape" | "delete" => {
                let control = if malformed == "escape" { 0x1b } else { 0x7f };
                let mut record = records(home).remove(0);
                let url = record
                    .windows(b"https".len())
                    .position(|window| window == b"https")
                    .unwrap();
                record.insert(url, control);
                frame(&token, "pre-sync", "eligible", "prepare", "1", &[record])
            }
            _ => frame(
                &token,
                "pre-sync",
                "eligible",
                "prepare",
                "1",
                &records(home),
            ),
        };
        match malformed {
            "magic" => body[0] = b'X',
            "version" => body[context::MAGIC.len() + 1] = b'2',
            "token-shape" => {
                presented = "z".repeat(64);
                let start = context::MAGIC.len() + 1 + context::VERSION.len() + 1;
                body[start..start + 64].copy_from_slice(presented.as_bytes());
            }
            "truncated" => {
                body.pop();
            }
            "trailing-nul" => body.push(0),
            "trailing-bytes" => body.extend_from_slice(b"tampered"),
            _ => {}
        }
        std::fs::write(&path, body).unwrap();
        assert_eq!(
            context::consume(&path, &presented, "pre-sync", home, euid(), now()),
            Err(context::Error::Refused),
            "{malformed}"
        );
        assert!(
            !path.exists(),
            "{malformed} authority was unlinked before decoder refusal"
        );
    }
}

#[test]
fn create_and_consume_enforce_absolute_private_paths_and_the_record_boundary() {
    let home = "/home/test";
    assert_eq!(
        context::create(
            Path::new("relative"),
            "merge",
            "active",
            "none",
            &records(home),
            home,
            euid(),
            now()
        ),
        Err(context::Error::Invalid(
            "context directory is not absolute: relative".to_string()
        ))
    );

    let dir = private_dir("context-boundary");
    let invalid = vec![b"work|relative|https://example.test/work.git|/home/test/.config/dot/overlays.d/10-work.conf|false|git".to_vec()];
    assert_eq!(
        context::create(
            dir.path(),
            "merge",
            "active",
            "none",
            &invalid,
            home,
            euid(),
            now()
        ),
        Err(context::Error::Invalid(
            "invalid overlay record".to_string()
        ))
    );

    let boundary: Vec<Vec<u8>> = (0..context::MAX_RECORDS)
        .map(|index| {
            format!(
                "o{index:03}|{home}/.dotfiles-o{index:03}|https://example.test/o{index:03}.git|{home}/.config/dot/overlays.d/10-o{index:03}.conf|false|git"
            )
            .into_bytes()
        })
        .collect();
    let (path, token) = context::create(
        dir.path(),
        "merge",
        "active",
        "none",
        &boundary,
        home,
        euid(),
        now(),
    )
    .unwrap();
    let decoded = context::consume(&path, &token, "merge", home, euid(), now()).unwrap();
    assert_eq!(decoded.records.len(), context::MAX_RECORDS);
    assert_eq!(
        decoded.records.first().unwrap(),
        &String::from_utf8(boundary[0].clone()).unwrap()
    );
    assert_eq!(
        decoded.records.last().unwrap(),
        &String::from_utf8(boundary.last().unwrap().clone()).unwrap()
    );

    assert_eq!(
        context::consume(
            Path::new("relative-context"),
            &"a".repeat(64),
            "merge",
            home,
            euid(),
            now()
        ),
        Err(context::Error::Refused)
    );

    let (path, token) = context::create(
        dir.path(),
        "merge",
        "active",
        "none",
        &records(home),
        home,
        euid(),
        now(),
    )
    .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        context::consume(&path, &token, "merge", home, euid(), now()),
        Err(context::Error::Refused)
    );
}

#[test]
fn create_rejects_unsafe_directory_matrix_duplicates_and_record_limits() {
    let home = "/home/test";
    let dir = private_dir("context-create-errors");
    let record = records(home);
    assert!(
        context::create(
            dir.path(),
            "merge",
            "eligible",
            "none",
            &record,
            home,
            euid(),
            now()
        )
        .is_err()
    );
    assert!(
        context::create(
            dir.path(),
            "merge",
            "active",
            "none",
            &[record[0].clone(), record[0].clone()],
            home,
            euid(),
            now()
        )
        .is_err()
    );
    let too_many = vec![record[0].clone(); context::MAX_RECORDS + 1];
    assert!(
        context::create(
            dir.path(),
            "merge",
            "active",
            "none",
            &too_many,
            home,
            euid(),
            now()
        )
        .is_err()
    );
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        context::create(
            dir.path(),
            "merge",
            "active",
            "none",
            &record,
            home,
            euid(),
            now()
        )
        .is_err()
    );
}
