//! BSD-`stat` simulator differential for replacement identities.
//!
//! Linux-only: the fake `stat` below normalizes the GNU coreutils
//! tool into BSD shape (`-c` fails, `-f '%d:%i'` / `'%p:%z'` render
//! BSD semantics), and shadows PATH so both engines take the BSD
//! branch. macOS needs no simulator — the main `repos_overlays`
//! suite exercises the BSD branch natively there (it caught the
//! original hex/octal mismatch). This binary runs a single test so
//! the process-PATH prepend cannot race a parallel probe.
//!
//! The mode pins (`100644`, `120777`) prove the BSD branch was
//! taken: the GNU branch would render `81a4` / `a1ff`.

#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use dot::repos_overlays;
use dot::test_support::TempDir;

/// Run one shell snippet with the manifest library sourced, like
/// `repos_overlays.rs` — but the caller prepends the fixture dir to
/// the process PATH first, so the child inherits the fake `stat`
/// through the preserved PATH below.
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
        .arg(format!(". \"$1/lib/dot/repos/overlays.sh\"\n{snippet}"));
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

/// A BSD-shaped `stat` over the GNU coreutils tool: `-c` always
/// fails (like BSD), while `-f` serves the identity formats the
/// overlay engine probes. Plain GNU `stat` never follows the leaf,
/// so file-type bits plus `stat -c '%a'` permission bits compose the
/// exact BSD `%p` octal rendering. Lives in an exec-capable fixture
/// dir (the system temp dir may be `noexec`).
fn fake_bsd_stat() -> TempDir {
    let dir = TempDir::new_exec("fake-stat").expect("exec dir");
    // Resolve the wrapped binary NOW, while this dir is on no PATH:
    // the test prepends the fixture dir to PATH, so a bare `stat`
    // inside the script would re-resolve to the fixture itself and
    // recurse.
    let real = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("stat"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .expect("real stat on PATH");
    let quoted = format!("'{}'", real.display().to_string().replace('\'', "'\\''"));
    let script = dir.path().join("stat");
    let body = "#!/bin/sh\n\
         REAL={REAL}\n\
         if [ \"$1\" = -c ]; then exit 1; fi\n\
         fmt=$2; file=$3\n\
         if [ -L \"$file\" ]; then type=120000\n\
         elif [ -d \"$file\" ]; then type=40000\n\
         elif [ -f \"$file\" ]; then type=100000\n\
         else exit 1; fi\n\
         mode() { perms=$(\"$REAL\" -c '%a' \"$file\") || exit 1\n\
           printf '%o' \"$((8#$type | 8#$perms))\"; }\n\
         case \"$fmt\" in\n\
           '%d:%i') exec \"$REAL\" -c '%d:%i' \"$file\" ;;\n\
           '%p') mode; printf '\\n' ;;\n\
           '%p:%z') size=$(\"$REAL\" -c '%s' \"$file\") || exit 1\n\
             printf '%s:%s\\n' \"$(mode)\" \"$size\" ;;\n\
           *) exit 1 ;;\n\
         esac\n"
        .replace("{REAL}", &quoted);
    std::fs::write(&script, body).expect("write fake stat");
    #[cfg(unix)]
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod fake");
    dir
}

#[test]
fn bsd_stat_octal_mode_agrees() {
    let stat_dir = fake_bsd_stat();
    // This binary runs no other test, so the process-PATH prepend
    // cannot race a parallel flavor probe.
    let path = std::env::join_paths(std::iter::once(stat_dir.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("join PATH");
    // SAFETY: this binary runs a single test, so no parallel thread
    // can observe the transient PATH.
    unsafe {
        std::env::set_var("PATH", &path);
    }
    let dir = TempDir::new("ovlink-bsd").expect("fixture dir");
    let home = dir.path();
    stage(home, "doc.txt", b"payload\n");
    std::os::unix::fs::symlink("doc.txt", home.join("link")).expect("symlink");
    // (name, BSD `%p` pin): octal proves the BSD branch — the GNU
    // branch would render `81a4` / `a1ff` here.
    for (name, mode_pin) in [("doc.txt", ":100644:8:"), ("link", ":120777:7:")] {
        let target = home.join(name);
        let (code, out, serr) = shell_run(
            home,
            &[target.as_os_str()],
            "out=$(_overlay_replacement_identity \"$2\"); code=$?; printf 'rc=%s\\nreply=%s\\n' \"$code\" \"$out\"\n",
        );
        assert_eq!(code, 0, "harness exit for {name:?}");
        assert!(
            serr.is_empty(),
            "BSD identity stderr for {name:?}: {serr:?}"
        );
        let shell = String::from_utf8(out).expect("identity dump");
        let rust = match repos_overlays::replacement_identity(home, &target) {
            Ok(identity) => format!("rc=0\nreply={identity}\n"),
            Err(_) => "rc=1\nreply=\n".to_string(),
        };
        assert_eq!(rust, shell, "BSD replacement identity for {name:?}");
        assert!(
            rust.contains(mode_pin),
            "BSD octal mode pin for {name:?}: {rust:?}"
        );
    }
}
