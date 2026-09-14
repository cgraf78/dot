//! Native contracts for base-repository topology and Git dispatch.
use dot::repos_base::{Base, Topology, overlay_path_sync, run_git};
use dot_test_support::TempDir;
use std::ffi::OsString;
use std::path::Path;
fn git(args: &[&std::ffi::OsStr]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .status()
            .unwrap()
            .success()
    )
}
fn ordinary(tag: &str) -> TempDir {
    let d = TempDir::new(tag).unwrap();
    git(&["init".as_ref(), "-q".as_ref(), d.path().as_os_str()]);
    d
}
fn separate() -> (TempDir, TempDir) {
    let home = TempDir::new("base-separate-home").unwrap();
    let gd = TempDir::new("base-separate-git").unwrap();
    git(&[
        "init".as_ref(),
        "--bare".as_ref(),
        "-q".as_ref(),
        gd.path().as_os_str(),
    ]);
    git(&[
        (format!("--git-dir={}", gd.path().display())).as_ref(),
        (format!("--work-tree={}", home.path().display())).as_ref(),
        "config".as_ref(),
        "core.bare".as_ref(),
        "false".as_ref(),
    ]);
    (home, gd)
}
fn base(topology: Topology, git: &Path, home: &Path) -> Base {
    Base {
        topology,
        client_git_dir: git.to_string_lossy().into(),
        home: home.to_string_lossy().into(),
    }
}

#[test]
fn exists_matrix() {
    for (t, w) in [
        (Topology::Missing, false),
        (Topology::Separate, true),
        (Topology::Ordinary, true),
    ] {
        assert_eq!(
            Base {
                topology: t,
                client_git_dir: "/g".into(),
                home: "/h".into()
            }
            .exists(),
            w
        )
    }
}
#[test]
fn git_prefix_argv() {
    assert_eq!(
        Base {
            topology: Topology::Separate,
            client_git_dir: "/g/dir".into(),
            home: "/home/u".into()
        }
        .git_prefix(),
        Some(vec![
            OsString::from("--git-dir=/g/dir"),
            OsString::from("--work-tree=/home/u")
        ])
    );
    assert_eq!(
        Base {
            topology: Topology::Ordinary,
            client_git_dir: "/g".into(),
            home: "/home/u".into()
        }
        .git_prefix(),
        Some(vec![OsString::from("-C"), OsString::from("/home/u")])
    );
    assert_eq!(
        Base {
            topology: Topology::Missing,
            client_git_dir: "/g".into(),
            home: "/h".into()
        }
        .git_prefix(),
        None
    )
}
#[test]
fn separate_show_toplevel() {
    let (home, gd) = separate();
    let out = run_git(
        &base(Topology::Separate, gd.path(), home.path())
            .git_prefix()
            .unwrap(),
        &["rev-parse", "--show-toplevel"],
    )
    .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim_end(),
        home.path().to_string_lossy()
    );
    assert!(out.stderr.is_empty())
}
#[test]
fn ordinary_show_toplevel() {
    let home = ordinary("base-ordinary");
    let out = run_git(
        &base(Topology::Ordinary, home.path(), home.path())
            .git_prefix()
            .unwrap(),
        &["rev-parse", "--show-toplevel"],
    )
    .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim_end(),
        home.path().to_string_lossy()
    )
}
#[test]
fn verify_failure_is_silent_and_nonzero_for_both_topologies() {
    let (home, gd) = separate();
    let ord = ordinary("base-fail-ordinary");
    for b in [
        base(Topology::Separate, gd.path(), home.path()),
        base(Topology::Ordinary, ord.path(), ord.path()),
    ] {
        let out = run_git(
            &b.git_prefix().unwrap(),
            &["rev-parse", "--verify", "no-such-ref"],
        )
        .unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert!(out.stderr.is_empty())
    }
}
#[test]
fn status_porcelain_reports_untracked_for_both_topologies() {
    let (home, gd) = separate();
    std::fs::write(home.path().join("scratch.txt"), b"x").unwrap();
    let ord = ordinary("base-status-ordinary");
    std::fs::write(ord.path().join("scratch.txt"), b"x").unwrap();
    for b in [
        base(Topology::Separate, gd.path(), home.path()),
        base(Topology::Ordinary, ord.path(), ord.path()),
    ] {
        let out = run_git(&b.git_prefix().unwrap(), &["status", "--porcelain"]).unwrap();
        assert!(out.status.success());
        assert!(
            out.stdout
                .split(|b| *b == b'\n')
                .any(|r| r == b"?? scratch.txt")
        )
    }
}
#[test]
fn missing_refuses_dispatch() {
    let d = TempDir::new("base-missing").unwrap();
    let b = base(Topology::Missing, d.path(), d.path());
    assert!(!b.exists());
    assert!(b.git_prefix().is_none())
}
fn cases() -> [(&'static str, &'static str, &'static str); 7] {
    [
        ("n|p|u|d|o|git", "p", "git"),
        ("n|p", "p", "git"),
        ("n|p|u|d|o|", "p", "git"),
        ("n|p|u|d|o|none", "p", "none"),
        ("n|p|u|d|o|git|x", "p", "git|x"),
        ("", "", "git"),
        ("plain-no-pipes", "", "git"),
    ]
}
#[test]
fn overlay_path_sync_matrix() {
    for (e, p, s) in cases() {
        assert_eq!(overlay_path_sync(e), (p.into(), s.into()))
    }
    assert_ne!(overlay_path_sync("n|p|u|d|o|git|x").1, "git")
}
#[test]
fn overlay_path_sync_preserves_last_field_remainder() {
    let (path, sync) = overlay_path_sync("n|with space|u|d|o|none|extra");
    assert_eq!(path, "with space");
    assert_eq!(sync, "none|extra")
}
#[test]
fn run_git_success_and_failure_contract() {
    let home = ordinary("base-probe");
    let prefix = base(Topology::Ordinary, home.path(), home.path())
        .git_prefix()
        .unwrap();
    let ok = run_git(&prefix, &["rev-parse", "--show-toplevel"]).unwrap();
    assert!(ok.status.success());
    assert!(ok.stdout.ends_with(b"\n"));
    assert!(ok.stderr.is_empty());
    let fail = run_git(&prefix, &["this-is-not-a-command"]).unwrap();
    assert!(!fail.status.success());
    assert!(fail.stderr.is_empty())
}
