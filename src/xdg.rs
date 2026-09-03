//! Public XDG base-directory resolver (slice 2 foundations).
//!
//! Ports `lib/dot/public/xdg.sh` exactly: empty and relative XDG values
//! fall back to HOME (accepting them would make runtime ownership depend
//! on cwd), and unknown kinds are usage errors. The shell reports via
//! `$REPLY` plus exit codes (0 ok, 2 usage, 1 unresolvable HOME); Rust
//! returns values and a coded error instead — no global `REPLY` exists
//! to keep in sync, which is precisely the class of state the port
//! eliminates.

/// Directory kinds the resolver knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `$XDG_CONFIG_HOME` or `~/.config`.
    Config,
    /// `$XDG_STATE_HOME` or `~/.local/state`.
    State,
    /// `$XDG_CACHE_HOME` or `~/.cache`.
    Cache,
    /// `$XDG_DATA_HOME` or `~/.local/share`.
    Data,
}

impl Kind {
    /// Parse a kind name; anything else is a usage error (shell `else`
    /// branch returns 2).
    pub fn parse(name: &str) -> Result<Self, Error> {
        match name {
            "config" => Ok(Kind::Config),
            "state" => Ok(Kind::State),
            "cache" => Ok(Kind::Cache),
            "data" => Ok(Kind::Data),
            _ => Err(Error::Usage),
        }
    }

    fn fallback(self) -> &'static str {
        match self {
            Kind::Config => ".config",
            Kind::State => ".local/state",
            Kind::Cache => ".cache",
            Kind::Data => ".local/share",
        }
    }
}

/// Resolver failure, carrying the shell's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Wrong kind or malformed suffix (shell exit 2).
    Usage,
    /// No absolute XDG value and no usable HOME (shell exit 1).
    Unresolvable,
}

impl Error {
    /// Shell exit code for this failure.
    pub fn code(self) -> i32 {
        match self {
            Error::Usage => 2,
            Error::Unresolvable => 1,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The shell prints no diagnostic for these paths (REPLY is
            // simply left empty); the code is the whole contract.
            Error::Usage => write!(f, "invalid XDG kind or suffix"),
            Error::Unresolvable => write!(f, "HOME does not provide an absolute base"),
        }
    }
}

impl std::error::Error for Error {}

/// Resolve the base directory for `kind`.
///
/// `xdg_value` is the raw `$XDG_*_HOME` (empty when unset): absolute
/// wins as-is — even `/` itself — otherwise HOME decides. `home` is raw
/// `$HOME`: `/` joins directly, any other absolute path gets the
/// fallback appended, anything else fails.
pub fn base(kind: Kind, xdg_value: &str, home_dir: &str) -> Result<String, Error> {
    if xdg_value.starts_with('/') {
        return Ok(xdg_value.to_string());
    }
    if home_dir == "/" {
        return Ok(format!("/{}", kind.fallback()));
    }
    if home_dir.starts_with('/') {
        return Ok(format!("{home_dir}/{}", kind.fallback()));
    }
    Err(Error::Unresolvable)
}

/// Reject path suffixes the shell refuses to join.
///
/// Mirrors the glob list literally: empty, absolute, trailing slash,
/// doubled slashes, any `.`/`..` segment, CR/LF anywhere. A `~` or `$`
/// suffix is NOT rejected here (the shell list does not exclude them);
//
/// that policy belongs to the config parser, not the joiner.
fn suffix_valid(suffix: &str) -> bool {
    if suffix.is_empty() {
        return false;
    }
    if suffix.contains('\n') || suffix.contains('\r') {
        return false;
    }
    // Any empty segment means leading, trailing, or doubled slashes;
    // any dot segment means `.`/`..` traversal.
    if suffix
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return false;
    }
    true
}

/// Join `suffix` onto the resolved base for `kind`.
pub fn path(kind: Kind, suffix: &str, xdg_value: &str, home_dir: &str) -> Result<String, Error> {
    if !suffix_valid(suffix) {
        return Err(Error::Usage);
    }
    let resolved = base(kind, xdg_value, home_dir)?;
    // The shell special-cases a `/` base to avoid `//suffix`.
    if resolved == "/" {
        Ok(format!("/{suffix}"))
    } else {
        Ok(format!("{resolved}/{suffix}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse_and_reject() {
        assert_eq!(Kind::parse("config"), Ok(Kind::Config));
        assert_eq!(Kind::parse("state"), Ok(Kind::State));
        assert_eq!(Kind::parse("cache"), Ok(Kind::Cache));
        assert_eq!(Kind::parse("data"), Ok(Kind::Data));
        for bad in ["", "Config", "CONFIG", "cache/", "xdg", "home"] {
            assert_eq!(Kind::parse(bad), Err(Error::Usage), "kind: {bad:?}");
        }
    }

    #[test]
    fn absolute_xdg_wins_verbatim() {
        // Even degenerate absolute values pass through: the shell's
        // `/*)` arm accepts before HOME is consulted.
        for (kind, var) in [
            (Kind::Config, "/c"),
            (Kind::State, "/"),
            (Kind::Cache, "/c a c h e"),
            (Kind::Data, "/d/"),
        ] {
            assert_eq!(base(kind, var, "/home/u"), Ok(var.to_string()));
            // HOME is not even read on this path.
            assert_eq!(base(kind, var, "relative"), Ok(var.to_string()));
        }
    }

    #[test]
    fn fallback_matrix() {
        let cases = [
            (Kind::Config, "/home/u/.config"),
            (Kind::State, "/home/u/.local/state"),
            (Kind::Cache, "/home/u/.cache"),
            (Kind::Data, "/home/u/.local/share"),
        ];
        for (kind, expected) in cases {
            // Empty and relative XDG values both fall back.
            for xdg in ["", "relative", ".config"] {
                assert_eq!(base(kind, xdg, "/home/u"), Ok(expected.to_string()));
            }
        }
        // Root HOME joins directly (no doubled slash).
        assert_eq!(base(Kind::Config, "", "/"), Ok("/.config".to_string()));
        // Unusable HOME fails with code 1.
        for bad_home in ["", "relative", "."] {
            assert_eq!(base(Kind::Config, "", bad_home), Err(Error::Unresolvable));
        }
        assert_eq!(Error::Unresolvable.code(), 1);
        assert_eq!(Error::Usage.code(), 2);
    }

    #[test]
    fn suffix_rejection_matrix() {
        let bad = [
            "", "/abs", "trail/", "a//b", ".", "./a", "a/./b", "a/.", "..", "../a", "a/../b",
            "a/..", "a\nb", "a\rb",
        ];
        for suffix in bad {
            assert_eq!(
                path(Kind::Config, suffix, "", "/home/u"),
                Err(Error::Usage),
                "suffix: {suffix:?}"
            );
        }
        // Survivors join onto the base (note: `~` is the config
        // parser's business, not the joiner's).
        let good = [
            ("dot/config", "/home/u/.config/dot/config"),
            ("a..b", "/home/u/.config/a..b"),
            ("~", "/home/u/.config/~"),
            ("a b", "/home/u/.config/a b"),
        ];
        for (suffix, expected) in good {
            assert_eq!(
                path(Kind::Config, suffix, "", "/home/u"),
                Ok(expected.to_string())
            );
        }
        // Absolute XDG base plus suffix.
        assert_eq!(
            path(Kind::State, "dot/lock", "/srv/state", "/home/u"),
            Ok("/srv/state/dot/lock".to_string())
        );
        // Slash base avoids `//suffix`.
        assert_eq!(
            path(Kind::State, "dot/lock", "/", "/home/u"),
            Ok("/dot/lock".to_string())
        );
    }
}
