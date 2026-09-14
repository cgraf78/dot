use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub struct Fixture {
    pub scope: dot_test_support::TempDir,
    pub home: PathBuf,
    pub suites: PathBuf,
    pub root: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let scope = dot_test_support::TempDir::new_exec("native-test").unwrap();
        let home = scope.path().join("home");
        let suites = scope.path().join("suites");
        let root = scope.path().join("provider");
        for path in [&home, &suites, &root, &scope.path().join("tmp")] {
            fs::create_dir_all(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
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
            .env("DOT_BASH", dot_test_support::bash())
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
}

pub fn executable(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!("#!{}\n{body}\n", dot_test_support::bash().display()),
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
