//! Native behavioral coverage for overlay inventory preparation.

use dot::repos_link_prep;
use dot_test_support::TempDir;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn stage(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}
fn git(cwd: &Path, home: &Path, args: &[&str]) {
    let mut command = Command::new(dot_test_support::real_tool("git"));
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home);
    let out = dot_test_support::isolate_git(&mut command)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}
fn git_overlay(root: &Path, home: &Path, name: &str) -> (PathBuf, String) {
    let source = root.join(format!("{name}-source"));
    stage(&source, "home/a.conf", b"a\n");
    stage(&source, "home/sub/b.conf", b"b\n");
    stage(&source, "home/stale.~1~", b"backup\n");
    stage(&source, "home/tilde~", b"kept\n");
    std::os::unix::fs::symlink("a.conf", source.join("home/link.conf")).unwrap();
    git(&source, home, &["init", "-b", "main"]);
    git(&source, home, &["add", "-A"]);
    git(&source, home, &["commit", "-qm", "seed"]);
    let checkout = root.join(name);
    git(
        root,
        home,
        &[
            "clone",
            "-q",
            source.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    (checkout, source.to_string_lossy().into_owned())
}
fn entry(name: &str, path: &Path, url: &str, sync: &str) -> String {
    format!("{name}|{}|{url}|||{sync}", path.display())
}
fn records(path: &Path) -> Vec<PathBuf> {
    let bytes = std::fs::read(path).unwrap();
    let mut paths: Vec<_> = bytes
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| PathBuf::from(std::ffi::OsStr::from_bytes(r)))
        .collect();
    paths.sort();
    paths
}

#[test]
fn prepares_git_inventory_with_private_mode_and_filters_backups() {
    let scope = TempDir::new("link-prep-git").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    let entries = vec![entry("overlay", &checkout, &url, "git")];
    let root = scope.path().join("inventories");
    std::fs::create_dir(&root).unwrap();
    let got = repos_link_prep::prepare_inventories(
        &repos_link_prep::Inputs {
            entries: &entries,
            home: home.to_str().unwrap(),
            update_jobs: Some("2"),
        },
        &root,
    )
    .unwrap();
    let inventory = &got.inventories["overlay"];
    assert_eq!(inventory.file_name().unwrap(), "1");
    assert_eq!(
        std::fs::metadata(inventory).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        records(inventory),
        vec![
            checkout.join("home/a.conf"),
            checkout.join("home/link.conf"),
            checkout.join("home/sub/b.conf"),
            checkout.join("home/tilde~")
        ]
    );
    assert!(got.source_roots.is_empty());
    assert!(got.source_identities.is_empty());
    assert!(
        std::fs::read_dir(&root).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .as_bytes()
            .starts_with(b".build-"))
    );
}

#[test]
fn local_inventory_freezes_physical_source_identity() {
    let scope = TempDir::new("link-prep-local").unwrap();
    let home = scope.path().join("home");
    let source = scope.path().join("local");
    stage(&source, "home/local.conf", b"local\n");
    std::fs::create_dir(&home).unwrap();
    let entries = vec![entry("local", &source, "", "none")];
    let root = scope.path().join("inventories");
    std::fs::create_dir(&root).unwrap();
    let got = repos_link_prep::prepare_inventories(
        &repos_link_prep::Inputs {
            entries: &entries,
            home: home.to_str().unwrap(),
            update_jobs: None,
        },
        &root,
    )
    .unwrap();
    assert_eq!(
        records(&got.inventories["local"]),
        vec![source.join("home/local.conf")]
    );
    assert_eq!(
        Path::new(&got.source_roots["local"]),
        std::fs::canonicalize(source.join("home")).unwrap()
    );
    assert!(!got.source_identities["local"].is_empty());
}

#[test]
fn skips_invalid_overlays_without_numbering_gaps() {
    let scope = TempDir::new("link-prep-skips").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (good, url) = git_overlay(scope.path(), &home, "good");
    let plain = scope.path().join("plain");
    stage(&plain, "home/x", b"x");
    let entries = vec![
        entry("missing", &scope.path().join("missing"), "", "git"),
        entry("plain", &plain, "ignored", "git"),
        entry("good", &good, &url, "git"),
    ];
    let root = scope.path().join("inventories");
    std::fs::create_dir(&root).unwrap();
    let got = repos_link_prep::prepare_inventories(
        &repos_link_prep::Inputs {
            entries: &entries,
            home: home.to_str().unwrap(),
            update_jobs: Some("bogus"),
        },
        &root,
    )
    .unwrap();
    assert_eq!(got.inventories.len(), 1);
    assert_eq!(got.inventories["good"].file_name().unwrap(), "1");
}

#[test]
fn parallel_preparation_is_repeatable() {
    let scope = TempDir::new("link-prep-parallel").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut entries = Vec::new();
    for name in ["one", "two", "three"] {
        let (checkout, url) = git_overlay(scope.path(), &home, name);
        entries.push(entry(name, &checkout, &url, "git"));
    }
    for round in 0..10 {
        let root = scope.path().join(format!("inventories-{round}"));
        std::fs::create_dir(&root).unwrap();
        let got = repos_link_prep::prepare_inventories(
            &repos_link_prep::Inputs {
                entries: &entries,
                home: home.to_str().unwrap(),
                update_jobs: Some("2"),
            },
            &root,
        )
        .unwrap();
        assert_eq!(got.inventories.len(), 3, "round {round}");
        for (name, index) in [("one", "1"), ("two", "2"), ("three", "3")] {
            assert_eq!(
                got.inventories[name].file_name().unwrap(),
                index,
                "round {round}: {name}"
            );
        }
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .as_bytes()
                .starts_with(b".build-")
        }));
    }
}

#[test]
fn missing_output_root_fails() {
    let scope = TempDir::new("link-prep-failure").unwrap();
    let home = scope.path().join("home");
    let source = scope.path().join("local");
    stage(&source, "home/a", b"a");
    std::fs::create_dir(&home).unwrap();
    let entries = vec![entry("local", &source, "", "none")];
    assert!(
        repos_link_prep::prepare_inventories(
            &repos_link_prep::Inputs {
                entries: &entries,
                home: home.to_str().unwrap(),
                update_jobs: None,
            },
            &scope.path().join("absent")
        )
        .is_none()
    );
}

#[test]
fn workers_run_the_dispatcher_bound_host_git() {
    // The host-Git binding is thread-local; inventory workers must carry
    // the dispatcher's selection instead of falling back to PATH lookup.
    let scope = TempDir::new_exec("link-prep-host-git").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    let entries = vec![entry("overlay", &checkout, &url, "git")];
    let root = scope.path().join("inventories");
    std::fs::create_dir(&root).unwrap();
    let log = scope.path().join("host-git.log");
    let real = [
        "/usr/bin/git",
        "/bin/git",
        "/usr/local/bin/git",
        "/opt/homebrew/bin/git",
    ]
    .into_iter()
    .map(Path::new)
    .find(|path| path.is_file())
    .expect("system git");
    let shim = scope.path().join("bin/git");
    std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
    dot_test_support::install_fixture_executable(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        ),
        0o755,
    )
    .unwrap();
    let got = dot::init_client_identity::with_host_git(&shim, || {
        repos_link_prep::prepare_inventories(
            &repos_link_prep::Inputs {
                entries: &entries,
                home: home.to_str().unwrap(),
                update_jobs: Some("2"),
            },
            &root,
        )
    })
    .unwrap();
    assert!(got.inventories.contains_key("overlay"));
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.contains(&format!("-C {}", checkout.display())),
        "worker Git bypassed the bound host Git: {calls:?}"
    );
}

/// Prepare one git overlay's inventory and return its sorted records.
fn git_inventory(scope: &TempDir, home: &Path, checkout: &Path, url: &str) -> Vec<PathBuf> {
    let entries = vec![entry("overlay", checkout, url, "git")];
    let root = scope.path().join("inventories");
    std::fs::create_dir(&root).unwrap();
    let got = repos_link_prep::prepare_inventories(
        &repos_link_prep::Inputs {
            entries: &entries,
            home: home.to_str().unwrap(),
            update_jobs: Some("2"),
        },
        &root,
    )
    .unwrap();
    records(&got.inventories["overlay"])
}

#[test]
fn git_inventory_leaves_out_untracked_and_ignored_files() {
    // A tool writing into the checkout (bytecode caches, editor swap
    // files) must not publish into $HOME: only files the overlay's index
    // tracks are linked.
    let scope = TempDir::new("link-prep-untracked").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    std::fs::write(checkout.join(".git/info/exclude"), "__pycache__/\n").unwrap();
    stage(&checkout, "home/bin/__pycache__/tool.pyc", b"bytecode\n");
    stage(&checkout, "home/notes.txt", b"scratch\n");
    stage(&checkout, "home/sub/.b.conf.swp", b"swap\n");
    assert_eq!(
        git_inventory(&scope, &home, &checkout, &url),
        vec![
            checkout.join("home/a.conf"),
            checkout.join("home/link.conf"),
            checkout.join("home/sub/b.conf"),
            checkout.join("home/tilde~")
        ]
    );
}

#[test]
fn git_inventory_includes_staged_files() {
    // The index is the authority: staging a new file publishes it before
    // the commit, and unstaging it withdraws it.
    let scope = TempDir::new("link-prep-staged").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    stage(&checkout, "home/new/staged.conf", b"staged\n");
    git(&checkout, &home, &["add", "home/new/staged.conf"]);
    git(&checkout, &home, &["rm", "-q", "--cached", "home/a.conf"]);
    assert_eq!(
        git_inventory(&scope, &home, &checkout, &url),
        vec![
            checkout.join("home/link.conf"),
            checkout.join("home/new/staged.conf"),
            checkout.join("home/sub/b.conf"),
            checkout.join("home/tilde~")
        ]
    );
}

#[test]
fn git_inventory_skips_tracked_paths_missing_or_replaced_on_disk() {
    // A tracked file deleted from the worktree has nothing to link, and a
    // tracked file replaced by a directory never publishes the directory's
    // untracked contents.
    let scope = TempDir::new("link-prep-missing").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    std::fs::remove_file(checkout.join("home/a.conf")).unwrap();
    std::fs::remove_file(checkout.join("home/sub/b.conf")).unwrap();
    stage(&checkout, "home/sub/b.conf/inner.conf", b"inner\n");
    assert_eq!(
        git_inventory(&scope, &home, &checkout, &url),
        vec![
            checkout.join("home/link.conf"),
            checkout.join("home/tilde~")
        ]
    );
}

#[test]
fn git_inventory_ignores_tracked_files_outside_home() {
    let scope = TempDir::new("link-prep-outside").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let source = scope.path().join("overlay-source");
    stage(&source, "home/a.conf", b"a\n");
    stage(&source, "homework/b.conf", b"b\n");
    stage(&source, "README.md", b"readme\n");
    git(&source, &home, &["init", "-b", "main"]);
    git(&source, &home, &["add", "-A"]);
    git(&source, &home, &["commit", "-qm", "seed"]);
    let checkout = scope.path().join("overlay");
    git(
        scope.path(),
        &home,
        &[
            "clone",
            "-q",
            source.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    assert_eq!(
        git_inventory(&scope, &home, &checkout, &source.to_string_lossy()),
        vec![checkout.join("home/a.conf")]
    );
}

#[test]
fn local_inventory_still_links_every_file() {
    // `sync=none` sources have no index: their whole tree is the overlay.
    let scope = TempDir::new("link-prep-local-all").unwrap();
    let home = scope.path().join("home");
    let source = scope.path().join("local");
    stage(&source, "home/local.conf", b"local\n");
    stage(&source, "home/bin/__pycache__/tool.pyc", b"bytecode\n");
    std::fs::create_dir(&home).unwrap();
    let entries = vec![entry("local", &source, "", "none")];
    let root = scope.path().join("inventories");
    std::fs::create_dir(&root).unwrap();
    let got = repos_link_prep::prepare_inventories(
        &repos_link_prep::Inputs {
            entries: &entries,
            home: home.to_str().unwrap(),
            update_jobs: None,
        },
        &root,
    )
    .unwrap();
    assert_eq!(
        records(&got.inventories["local"]),
        vec![
            source.join("home/bin/__pycache__/tool.pyc"),
            source.join("home/local.conf")
        ]
    );
}

#[test]
fn git_inventory_never_follows_a_symlinked_tracked_directory() {
    // The index can still list `home/sub/b.conf` after `home/sub` became
    // a symlink; following it would publish files from outside the
    // checkout, which the walk never did either.
    let scope = TempDir::new("link-prep-symlinked-dir").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    let outside = scope.path().join("outside");
    stage(&outside, "b.conf", b"outside\n");
    std::fs::remove_dir_all(checkout.join("home/sub")).unwrap();
    std::os::unix::fs::symlink(&outside, checkout.join("home/sub")).unwrap();
    assert_eq!(
        git_inventory(&scope, &home, &checkout, &url),
        vec![
            checkout.join("home/a.conf"),
            checkout.join("home/link.conf"),
            checkout.join("home/tilde~")
        ]
    );
}

#[test]
fn git_inventory_lists_a_conflicted_path_once() {
    // The index holds one entry per merge stage for a conflicted path;
    // linking it once per stage would record it three times.
    let scope = TempDir::new("link-prep-conflict").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    for (branch, body) in [("side", "side\n"), ("main", "main\n")] {
        if branch == "side" {
            git(&checkout, &home, &["checkout", "-q", "-b", "side"]);
        } else {
            git(&checkout, &home, &["checkout", "-q", "main"]);
        }
        std::fs::write(checkout.join("home/a.conf"), body).unwrap();
        git(
            &checkout,
            &home,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qam",
                branch,
            ],
        );
    }
    let merge = Command::new(dot_test_support::real_tool("git"))
        .current_dir(&checkout)
        .env("HOME", &home)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "merge",
            "-q",
            "side",
        ])
        .output()
        .unwrap();
    assert!(!merge.status.success(), "the merge should conflict");
    assert_eq!(
        git_inventory(&scope, &home, &checkout, &url),
        vec![
            checkout.join("home/a.conf"),
            checkout.join("home/link.conf"),
            checkout.join("home/sub/b.conf"),
            checkout.join("home/tilde~")
        ]
    );
}

#[test]
fn git_inventory_links_one_file_once_across_case_spellings() {
    // On a case-insensitive volume `README` and `readme` in the index name
    // one file; a hardlink stands in for that alias here. A differently
    // named hardlink is a separate path and still links.
    let scope = TempDir::new("link-prep-case").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    for name in ["README", "readme", "other"] {
        stage(&checkout, &format!("home/{name}"), name.as_bytes());
    }
    git(
        &checkout,
        &home,
        &["add", "home/README", "home/readme", "home/other"],
    );
    for alias in ["readme", "other"] {
        std::fs::remove_file(checkout.join("home").join(alias)).unwrap();
        std::fs::hard_link(
            checkout.join("home/README"),
            checkout.join("home").join(alias),
        )
        .unwrap();
    }
    let records = git_inventory(&scope, &home, &checkout, &url);
    assert!(
        records.contains(&checkout.join("home/README")),
        "{records:?}"
    );
    assert!(
        !records.contains(&checkout.join("home/readme")),
        "{records:?}"
    );
    assert!(
        records.contains(&checkout.join("home/other")),
        "{records:?}"
    );
}

#[test]
fn git_inventory_refuses_a_missing_index_over_a_populated_home() {
    // A deleted index lists nothing and exits zero; reading that as "the
    // overlay ships nothing" would remove every one of its links.
    let scope = TempDir::new("link-prep-no-index").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    std::fs::remove_file(checkout.join(".git/index")).unwrap();
    let entries = vec![entry("overlay", &checkout, &url, "git")];
    let root = scope.path().join("inventories");
    std::fs::create_dir(&root).unwrap();
    assert!(
        repos_link_prep::prepare_inventories(
            &repos_link_prep::Inputs {
                entries: &entries,
                home: home.to_str().unwrap(),
                update_jobs: None,
            },
            &root,
        )
        .is_none()
    );
}

#[test]
fn git_inventory_keeps_only_home_records_under_case_insensitive_pathspecs() {
    let scope = TempDir::new_exec("link-prep-icase").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    stage(&checkout, "Home/other.conf", b"other\n");
    git(&checkout, &home, &["add", "Home/other.conf"]);
    let shim = scope.path().join("bin/git");
    std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
    dot_test_support::install_fixture_executable(
        &shim,
        format!(
            "#!/bin/sh\nGIT_ICASE_PATHSPECS=1 exec '{}' \"$@\"\n",
            dot_test_support::real_tool("git").display()
        ),
        0o755,
    )
    .unwrap();
    let records = dot::init_client_identity::with_host_git(&shim, || {
        git_inventory(&scope, &home, &checkout, &url)
    });
    assert_eq!(
        records,
        vec![
            checkout.join("home/a.conf"),
            checkout.join("home/link.conf"),
            checkout.join("home/sub/b.conf"),
            checkout.join("home/tilde~")
        ]
    );
}

#[test]
fn git_inventory_of_an_overlay_tracking_nothing_under_home_is_empty() {
    // Upstream dropped every file under `home/`, but an untracked leftover
    // keeps the directory: the index is intact, so the overlay simply
    // ships nothing (only a missing index is refused).
    let scope = TempDir::new("link-prep-tracks-nothing").unwrap();
    let home = scope.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let (checkout, url) = git_overlay(scope.path(), &home, "overlay");
    git(&checkout, &home, &["rm", "-q", "-r", "home"]);
    stage(&checkout, "home/.DS_Store", b"finder\n");
    assert_eq!(
        git_inventory(&scope, &home, &checkout, &url),
        Vec::<PathBuf>::new()
    );
}
