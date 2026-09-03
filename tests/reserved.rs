//! Differential parity tests for reserved control-plane paths against
//! `lib/dot/reserved.sh`: the roots inventory, leaf reservation, and
//! candidate reservation (including the ancestor-swallowing rule).

use std::process::{Command, Stdio};

use dot::reserved::{
    RootsInput, candidate_path_is_reserved_from_roots, path_is_reserved_from_roots, reserved_roots,
};

/// Oracle interpreter, shared with the other differential harnesses (see
/// `dot::test_support::bash`).
fn bash_bin() -> &'static std::path::Path {
    dot::test_support::bash()
}

/// Fixture client root: a real dotfiles dir behind a symlink (to pin
/// the leaf-symlink second-root branch), state dir, one directory
/// overlay plus one DANGLING symlink overlay (to pin `realpath`
/// semantics for leaves whose target is absent), and an init backup
/// target.
struct Fixture {
    dir: dot::test_support::TempDir,
    home: String,
}

impl Fixture {
    fn build() -> Self {
        let dir = dot::test_support::TempDir::new("reserved").expect("temp dir");
        let home = dir.path().to_string_lossy().into_owned();
        let real = dir.path().join("real-dotfiles");
        std::fs::create_dir_all(&real).expect("mkdir");
        std::fs::create_dir_all(dir.path().join(".local/state/dot")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join(".local/share")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("overlays/ov1")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("backup")).expect("mkdir");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, dir.path().join(".dotfiles")).expect("symlink");
            std::os::unix::fs::symlink(
                dir.path().join("overlays/gone"),
                dir.path().join("overlays/ov-dangling"),
            )
            .expect("symlink");
        }
        Self { dir, home }
    }

    fn overlay_paths(&self) -> Vec<String> {
        vec![
            format!("{}/overlays/ov1", self.home),
            format!("{}/overlays/ov-dangling", self.home),
        ]
    }

    fn input(&self) -> RootsInput {
        RootsInput {
            home: self.home.clone(),
            state_home: format!("{}/.local/state", self.home),
            install_root: format!("{}/.local/share", self.home),
            provider_state: format!("{}/.local/state/shdeps", self.home),
            overlay_paths: self.overlay_paths(),
            init_backup: Some(format!("{}/backup", self.home)),
        }
    }

    /// `OVERLAYS=(...)` assignment covering every fixture overlay
    /// record (single-quoted, so scratch paths with spaces survive).
    fn overlays_assignment(&self) -> String {
        let records: Vec<String> = self
            .overlay_paths()
            .iter()
            .map(|path| {
                let record = format!("base|{path}|https://example.invalid/x|git||git");
                format!("'{}'", record.replace('\'', "'\\''"))
            })
            .collect();
        format!("OVERLAYS=({})", records.join(" "))
    }

    /// Run `dot_path_is_reserved` or `dot_candidate_path_is_reserved`
    /// in a child whose HOME, XDG state, SHDEPS roots, overlay records,
    /// and backup match [`Fixture::input`]. Returns the exit code.
    fn shell_check(&self, function: &str, path: &str) -> i32 {
        let overlays = self.overlays_assignment();
        let script = format!(
            ". \"$1/lib/dot/public/xdg.sh\"\n\
             . \"$1/lib/dot/temp.sh\"\n\
             . \"$1/lib/dot/reserved.sh\"\n\
             {overlays}\n\
             {function} \"$2\"\n"
        );
        let output = Command::new(bash_bin())
            .arg("--noprofile")
            .arg("--norc")
            .arg("-c")
            .arg(script)
            .arg("dot-test-sh")
            .arg(env!("CARGO_MANIFEST_DIR"))
            .arg(path)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("LC_ALL", "C")
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", format!("{}/.local/state", self.home))
            .env("SHDEPS_INSTALL_DIR", format!("{}/.local/share", self.home))
            .env("DOT_INIT_BACKUP", format!("{}/backup", self.home))
            .current_dir(self.dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .expect("spawn bash");
        output.status.code().unwrap_or(99)
    }
}

#[test]
fn rust_matches_shell_on_roots_snapshot() {
    let fixture = Fixture::build();
    let input = fixture.input();
    let overlays = fixture.overlays_assignment();
    let script = format!(
        ". \"$1/lib/dot/public/xdg.sh\"\n\
        . \"$1/lib/dot/temp.sh\"\n\
        . \"$1/lib/dot/reserved.sh\"\n\
        {overlays}\n\
        _dot_reserved_roots\n"
    );
    let output = Command::new(bash_bin())
        .arg("--noprofile")
        .arg("--norc")
        .arg("-c")
        .arg(script)
        .arg("dot-test-sh")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .env("HOME", &fixture.home)
        .env("XDG_STATE_HOME", format!("{}/.local/state", fixture.home))
        .env(
            "SHDEPS_INSTALL_DIR",
            format!("{}/.local/share", fixture.home),
        )
        .env("DOT_INIT_BACKUP", format!("{}/backup", fixture.home))
        .current_dir(fixture.dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawn bash");
    assert_eq!(output.status.code(), Some(0), "snapshot failed");
    let shell_lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let pwd = fixture.dir.path().to_string_lossy().into_owned();
    let rust = reserved_roots(&input, &pwd).expect("snapshot");
    assert_eq!(rust, shell_lines, "roots inventory divergence");
}

#[test]
fn rust_matches_shell_on_reservation_verdicts() {
    let fixture = Fixture::build();
    let input = fixture.input();
    let pwd = fixture.dir.path().to_string_lossy().into_owned();
    let roots = reserved_roots(&input, &pwd).expect("snapshot");
    let checkout = format!("{}/.local/share/cgraf78/dot", fixture.home);
    let h = &fixture.home;
    let cases = [
        // Inside reserved roots.
        format!("{h}/.local/state/dot/overlay-links"),
        format!("{h}/.local/state/shdeps/x"),
        format!("{h}/.dotfiles/config"),
        format!("{h}/real-dotfiles/config"),
        format!("{h}/overlays/ov1/payload"),
        // Through the dangling overlay: reserved via the link row and
        // via the target-location second row alike.
        format!("{h}/overlays/ov-dangling/payload"),
        format!("{h}/overlays/gone/payload"),
        format!("{h}/backup/snap"),
        format!("{h}/.local/bin/dot"),
        format!("{h}/.local/share/cgraf78/dot/lib/x"),
        // Installer transients.
        format!("{h}/.local/share/cgraf78/.dot.clone.1"),
        format!("{h}/.local/share/cgraf78/dot.tmp.2"),
        format!("{h}/.local/share/cgraf78/.dot.install.lock"),
        // Recovery sentinels.
        format!("{h}/.dot-init-entry.1/x"),
        format!("{h}/sub/.dot-init-parent.2"),
        // Ancestor of a root: a plain leaf check passes it, the
        // candidate check refuses it (it would swallow `.dotfiles`).
        h.clone(),
        format!("{h}/.local"),
        // Missing-suffix route (parent does not exist yet).
        format!("{h}/no-such-dir/nested/file"),
        // Unrelated paths.
        format!("{h}/other/file"),
        "/tmp/dot-reserved-probe-xyz".to_string(),
        format!("{h}/.dotfiles-backup"),
    ];
    for path in &cases {
        let shell_leaf = fixture.shell_check("dot_path_is_reserved", path);
        // Shell exit 0 means reserved; Rust answers bool.
        let rust_leaf = if path_is_reserved_from_roots(path, &roots, h, &checkout) {
            0
        } else {
            1
        };
        assert_eq!(
            rust_leaf, shell_leaf,
            "leaf divergence path={path:?}: roots={roots:?}"
        );
        let shell_candidate = fixture.shell_check("dot_candidate_path_is_reserved", path);
        let rust_candidate =
            if candidate_path_is_reserved_from_roots(path, &roots, h, &checkout, &pwd) {
                0
            } else {
                1
            };
        assert_eq!(
            rust_candidate, shell_candidate,
            "candidate divergence path={path:?}: roots={roots:?}"
        );
    }
}

#[test]
fn arity_errors_are_exit_two() {
    let fixture = Fixture::build();
    assert_eq!(
        fixture.shell_check("dot_path_is_reserved", "relative/path"),
        2
    );
    assert_eq!(
        fixture.shell_check("dot_candidate_path_is_reserved", "relative/path"),
        2
    );
}
