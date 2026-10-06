//! Shdeps provider environment.
//!
//! Resolves the directories and flags the Shdeps provider runs with
//! (`_dot_shdeps_configure_env`). Provider selection, the ABI probe,
//! bootstrap, and re-exec orchestration live in the private
//! `shdeps_provider` coordinator.
//!
//! The `DOT_FORCE` / `DOT_QUIET` flags accept decimal spellings only —
//! the shell's `-eq` also honors hex like `0x1`, which stays
//! unreproduced.

/// Raw inputs for [`configure_env`], mirroring the exact environment
/// the shell reads: empty strings behave like unset for the
/// defaulted directories (`${VAR:-default}`), while the XDG and home
/// values arrive raw (empty when unset), like `checkpoint_path` in
/// part 2 takes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigureInputs<'a> {
    /// Raw `$XDG_CONFIG_HOME` (empty when unset).
    pub xdg_config_home: &'a str,
    /// Raw `$HOME`.
    pub home: &'a str,
    /// Raw `$SHDEPS_INSTALL_DIR` (empty falls back to the default).
    pub install_dir: &'a str,
    /// Raw `$SHDEPS_BIN_DIR` (empty falls back to the default).
    pub bin_dir: &'a str,
    /// Raw `$SHDEPS_GIT_DEV_DIR` (empty falls back to the default).
    pub git_dev_dir: &'a str,
    /// Raw `$DOT_FORCE` (empty behaves like `"0"`).
    pub dot_force: &'a str,
    /// Raw `$DOT_QUIET` (empty behaves like `"0"`).
    pub dot_quiet: &'a str,
}

/// Configured provider environment from [`configure_env`]: each
/// field is the value the shell exports under the matching `SHDEPS_*`
/// name, except `force` / `quiet`, which report whether the shell
/// exports `SHDEPS_FORCE=1` / `SHDEPS_QUIET=1` (`false` leaves any
/// prior caller value alone — the shell only ever sets these).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredEnv {
    /// Directory the shell exports as `SHDEPS_CONF_DIR`.
    pub conf_dir: String,
    /// Directory the shell exports as `SHDEPS_HOOKS_DIR`.
    pub hooks_dir: String,
    /// Directory the shell exports as `SHDEPS_INSTALL_DIR`.
    pub install_dir: String,
    /// Directory the shell exports as `SHDEPS_BIN_DIR`.
    pub bin_dir: String,
    /// Directory the shell exports as `SHDEPS_GIT_DEV_DIR`.
    pub git_dev_dir: String,
    /// Whether the shell exports `SHDEPS_FORCE=1`.
    pub force: bool,
    /// Whether the shell exports `SHDEPS_QUIET=1`.
    pub quiet: bool,
}

/// Whether `raw` enables a `DOT_FORCE` / `DOT_QUIET` style flag, like
/// the shell `[[ "${VAR:-0}" -eq 1 ]]`: decimal `1` (with optional
/// sign and surrounding whitespace, which the shell's arithmetic
/// tolerates) enables; everything else — including empty,
/// non-numeric, and hex spellings the shell arithmetic would honor —
/// refuses.
fn dot_flag(raw: &str) -> bool {
    match raw.trim().parse::<i128>() {
        Ok(value) => value == 1,
        Err(_) => false,
    }
}

/// `_dot_shdeps_configure_env`: resolve the provider directories and
/// flags, or `None` when the `dot_xdg_path config shdeps` root is
/// unresolvable, like the shell `return 1`. Success always reports
/// `Some`, including the ordinary non-force, non-quiet path the
/// shell pins with its trailing `return 0`.
pub fn configure_env(inputs: &ConfigureInputs<'_>) -> Option<ConfiguredEnv> {
    let conf_dir = crate::xdg::path(
        crate::xdg::Kind::Config,
        "shdeps",
        inputs.xdg_config_home,
        inputs.home,
    )
    .ok()?;
    let hooks_dir = format!("{conf_dir}/hooks.d");
    let install_dir = if inputs.install_dir.is_empty() {
        format!("{}/.local/share", inputs.home)
    } else {
        inputs.install_dir.to_string()
    };
    let bin_dir = if inputs.bin_dir.is_empty() {
        format!("{}/.local/bin", inputs.home)
    } else {
        inputs.bin_dir.to_string()
    };
    let git_dev_dir = if inputs.git_dev_dir.is_empty() {
        format!("{}/git", inputs.home)
    } else {
        inputs.git_dev_dir.to_string()
    };
    Some(ConfiguredEnv {
        conf_dir,
        hooks_dir,
        install_dir,
        bin_dir,
        git_dev_dir,
        force: dot_flag(inputs.dot_force),
        quiet: dot_flag(inputs.dot_quiet),
    })
}
