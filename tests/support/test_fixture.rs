use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub struct Fixture {
    pub scope: dot::test_support::TempDir,
    pub home: PathBuf,
    pub suites: PathBuf,
    pub root: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let scope = dot::test_support::TempDir::new_exec("native-test").unwrap();
        let home = scope.path().join("home");
        let suites = scope.path().join("suites");
        let root = scope.path().join("provider");
        for path in [&home, &suites, &root, &scope.path().join("tmp")] {
            fs::create_dir_all(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(
            Command::new("cp")
                .args(["-R", &format!("{}/lib", env!("CARGO_MANIFEST_DIR"))])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        for name in [
            "test.sh",
            "test/source.sh",
            "test/discovery.sh",
            "test/runner.sh",
        ] {
            fs::write(
                root.join("lib/dot").join(name),
                "echo POISON_TEST_ENGINE >&2; exit 97\n",
            )
            .unwrap();
        }
        Self {
            scope,
            home,
            suites,
            root,
        }
    }

    pub fn suite(&self, name: &str, body: &str) {
        executable(&self.suites.join(format!("{name}-test")), body);
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_dot"));
        cmd.arg("test")
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", &self.home)
            .env("TMPDIR", self.scope.path().join("tmp"))
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("DOT_SOURCE_ROOT", &self.root)
            .env("DOT_TEST_TESTS_DIR", &self.suites)
            .env("DOT_TEST_NO_COLOR", "1")
            .env("DOT_BASH", dot::test_support::bash())
            .env("LC_ALL", "C")
            .current_dir(&self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    #[allow(dead_code)] // Shared by integration binaries with different entry paths.
    pub fn run(&self, args: &[&str]) -> Output {
        finish(self.command(args).spawn().unwrap())
    }

    pub fn oracle(&self, name: &str) -> Output {
        // The provider oracle copies its checkout and asserts safe.directory
        // behavior. Give the copy its own repository instead of letting Git
        // walk upward into this integration test's enclosing source checkout.
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .arg(&self.root)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("cp")
                .args(["-R", &format!("{}/tests", env!("CARGO_MANIFEST_DIR"))])
                .arg(&self.root)
                .status()
                .unwrap()
                .success()
        );
        // The shell contract tests call one pure timeout-default helper from
        // test.sh; poison its executable coordinator while retaining that API.
        if name == "test-command-test" {
            let mut text =
                fs::read_to_string(format!("{}/lib/dot/test.sh", env!("CARGO_MANIFEST_DIR")))
                    .unwrap();
            text.push_str("\ndot_test_command() { echo POISON_TEST_ENGINE >&2; exit 97; }\n");
            fs::write(self.root.join("lib/dot/test.sh"), text).unwrap();
            let script = self.root.join("tests/test-command-test");
            let text = fs::read_to_string(&script).unwrap().replace("assert_eq 0 \"$parallel_ci_status\"", "[[ $parallel_ci_status == 0 ]] || printf '%s\\n' \"$parallel_ci_output\" >&2\nassert_eq 0 \"$parallel_ci_status\"");
            fs::write(script, text).unwrap();
        }
        executable(
            &self.root.join("bin/dot"),
            &format!(
                "DOT_SOURCE_ROOT=$(cd -P -- \"${{BASH_SOURCE[0]%/*}}/..\" && pwd -P)\nexport DOT_SOURCE_ROOT\nexec '{}' \"$@\"",
                env!("CARGO_BIN_EXE_dot")
            ),
        );
        Command::new(dot::test_support::bash())
            .arg(self.root.join("tests").join(name))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", &self.home)
            .env("TMPDIR", self.scope.path().join("tmp"))
            .env("LC_ALL", "C")
            .env("DOT_TEST_NO_COLOR", "1")
            .env("DOT_BASH", dot::test_support::bash())
            .current_dir(&self.home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

pub fn executable(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!("#!{}\n{body}\n", dot::test_support::bash().display()),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

pub fn poll(mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < end, "observable condition timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn finish(mut child: Child) -> Output {
    let end = Instant::now() + Duration::from_secs(25);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            panic!("dot test did not finish within deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

pub fn success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
