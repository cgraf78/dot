//! Native contracts for the Shdeps provider environment. The provider ABI
//! probe is pinned in `src/shdeps_provider.rs` and `tests/shdeps_provider.rs`.

use dot::shdeps_env_abi::{ConfigureInputs, ConfiguredEnv, configure_env};

fn expect_config(inputs: ConfigureInputs<'_>, expected: Option<ConfiguredEnv>) {
    assert_eq!(configure_env(&inputs), expected, "inputs: {inputs:?}");
}

fn inputs<'a>(
    xdg: &'a str,
    home: &'a str,
    install: &'a str,
    bin: &'a str,
    git: &'a str,
    force: &'a str,
    quiet: &'a str,
) -> ConfigureInputs<'a> {
    ConfigureInputs {
        xdg_config_home: xdg,
        home,
        install_dir: install,
        bin_dir: bin,
        git_dev_dir: git,
        dot_force: force,
        dot_quiet: quiet,
    }
}

#[test]
fn configure_defaults_and_xdg() {
    expect_config(
        inputs("", "/home/tester", "", "", "", "", ""),
        Some(ConfiguredEnv {
            conf_dir: "/home/tester/.config/shdeps".into(),
            hooks_dir: "/home/tester/.config/shdeps/hooks.d".into(),
            install_dir: "/home/tester/.local/share".into(),
            bin_dir: "/home/tester/.local/bin".into(),
            git_dev_dir: "/home/tester/git".into(),
            force: false,
            quiet: false,
        }),
    );
    expect_config(
        inputs("/var/config", "/home/tester", "", "", "", "", ""),
        Some(ConfiguredEnv {
            conf_dir: "/var/config/shdeps".into(),
            hooks_dir: "/var/config/shdeps/hooks.d".into(),
            install_dir: "/home/tester/.local/share".into(),
            bin_dir: "/home/tester/.local/bin".into(),
            git_dev_dir: "/home/tester/git".into(),
            force: false,
            quiet: false,
        }),
    );
    expect_config(
        inputs("rel/conf", "/home/tester", "", "", "", "", ""),
        Some(ConfiguredEnv {
            conf_dir: "/home/tester/.config/shdeps".into(),
            hooks_dir: "/home/tester/.config/shdeps/hooks.d".into(),
            install_dir: "/home/tester/.local/share".into(),
            bin_dir: "/home/tester/.local/bin".into(),
            git_dev_dir: "/home/tester/git".into(),
            force: false,
            quiet: false,
        }),
    );
    expect_config(
        inputs("", "/", "", "", "", "", ""),
        Some(ConfiguredEnv {
            conf_dir: "/.config/shdeps".into(),
            hooks_dir: "/.config/shdeps/hooks.d".into(),
            install_dir: "//.local/share".into(),
            bin_dir: "//.local/bin".into(),
            git_dev_dir: "//git".into(),
            force: false,
            quiet: false,
        }),
    );
}

#[test]
fn configure_overrides() {
    for (install, bin, git, expected_install, expected_bin, expected_git) in [
        (
            "/opt/shdeps",
            "/opt/bin",
            "/opt/git",
            "/opt/shdeps",
            "/opt/bin",
            "/opt/git",
        ),
        (
            "",
            "",
            "",
            "/home/tester/.local/share",
            "/home/tester/.local/bin",
            "/home/tester/git",
        ),
        (
            "/opt/shdeps",
            "",
            "/opt/git",
            "/opt/shdeps",
            "/home/tester/.local/bin",
            "/opt/git",
        ),
    ] {
        expect_config(
            inputs("", "/home/tester", install, bin, git, "", ""),
            Some(ConfiguredEnv {
                conf_dir: "/home/tester/.config/shdeps".into(),
                hooks_dir: "/home/tester/.config/shdeps/hooks.d".into(),
                install_dir: expected_install.into(),
                bin_dir: expected_bin.into(),
                git_dev_dir: expected_git.into(),
                force: false,
                quiet: false,
            }),
        );
    }
    expect_config(inputs("", "relative-home", "", "", "", "", ""), None);
}

#[test]
fn configure_force_quiet_flags() {
    for (force, quiet, expected_force, expected_quiet) in [
        ("0", "0", false, false),
        ("1", "", true, false),
        ("", "1", false, true),
        ("1", "1", true, true),
        ("", "", false, false),
        ("2", "yes", false, false),
        (" 1 ", "+1", true, true),
        ("01", "00", true, false),
        ("0x1", "1.0", false, false),
    ] {
        let configured = configure_env(&inputs("", "/home/tester", "", "", "", force, quiet))
            .expect("absolute home configures");
        assert_eq!(configured.force, expected_force, "force {force:?}");
        assert_eq!(configured.quiet, expected_quiet, "quiet {quiet:?}");
    }
}
