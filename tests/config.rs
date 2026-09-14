//! Native config-parser contract tests over accepted and rejected corpora.

use std::path::Path;
use std::process::Command;

use dot::config::{
    Config, Provider, Request, UpdatePolicy, config_control_bytes, config_error,
    extensions_enabled, load,
};
use dot::xdg::{self, Kind};
use dot_test_support::TempDir;

/// Fail loudly when scratch storage loses a fixture before parsing: a missing
/// file legitimately returns defaults and could otherwise hide a broken setup.
fn require_fixture(path: &Path, context: &str) {
    assert!(
        path.is_file(),
        "fixture vanished before {context}: {}",
        path.display()
    );
}

/// Observable CLI-facing parse outcome.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Ok(String),
    Err(String),
}

fn rust_parse(config_path: &Path, home: &str, env_policy: Option<&str>) -> Outcome {
    match load(&Request {
        config_path: Some(config_path),
        home,
        env_policy,
    }) {
        Ok(config) => Outcome::Ok(format!(
            "{}|{}|{}|{}|{}|{}\n",
            config.version,
            if config.extension_api { "1" } else { "" },
            config.extensions_dir.as_deref().unwrap_or(""),
            match config.provider {
                Provider::None => "none",
                Provider::Shdeps => "shdeps",
            },
            config.default_profile,
            match config.shdeps_update_policy {
                UpdatePolicy::Pinned => "pinned",
                UpdatePolicy::Latest => "latest",
            },
        )),
        Err(err) => Outcome::Err(format!("{err}\n")),
    }
}

/// Valid-config corpus with independently specified public fields.
const VALID_CORPUS: &[(&str, &str)] = &[
    ("version=1\n", "1|||none|base|pinned\n"),
    ("version=1", "1|||none|base|pinned\n"),
    ("# only a comment\n\nversion=1\n", "1|||none|base|pinned\n"),
    (
        "version=1\nextension_api=1\nextensions_dir=${HOME}/.local/lib/dotfiles\ndependency_provider=shdeps\ndefault_profile=dev\nshdeps_update_policy=latest\n",
        "1|1|/home/tester/.local/lib/dotfiles|shdeps|dev|latest\n",
    ),
    (
        "version=1\nextension_api=1\nextensions_dir=~/x\n",
        "1|1|/home/tester/x|none|base|pinned\n",
    ),
    (
        "version=1\nextension_api=1\nextensions_dir=$HOME/x\n",
        "1|1|/home/tester/x|none|base|pinned\n",
    ),
    (
        "version=1\nextension_api=1\nextensions_dir=/srv/dotfiles\n",
        "1|1|/srv/dotfiles|none|base|pinned\n",
    ),
    (
        "version=1\ndefault_profile=a-b-c-9\n",
        "1|||none|a-b-c-9|pinned\n",
    ),
    (
        "version=1\nshdeps_update_policy=pinned\n",
        "1|||none|base|pinned\n",
    ),
    (
        "version=1\ndependency_provider=none\n",
        "1|||none|base|pinned\n",
    ),
];

/// Invalid-config corpus with the exact public diagnostic for every row.
const INVALID_CORPUS: &[(&str, &str)] = &[
    ("extension_api=1\n", "version=1 must be the first setting"),
    ("version=2\n", "unsupported version: 2"),
    ("version=1\nversion=1\n", "duplicate version"),
    ("version=1\nbogus=1\n", "unknown key: bogus"),
    ("version=1\nunknown=value\n", "unknown key: unknown"),
    ("version=1\nBad_Key=1\n", "line 2 has an invalid key"),
    ("version=1\n=1\n", "line 2 has an invalid key"),
    ("version\n", "line 1 is not key=value"),
    ("not a setting\n", "line 1 is not key=value"),
    ("version=1\\\n", "line 1 uses a continuation"),
    (
        "version=1\nextension_api=1\nextension_api=1\n",
        "duplicate extension_api",
    ),
    (
        "version=1\nextension_api=yes\n",
        "unsupported extension_api: yes",
    ),
    (
        "version=1\nextensions_dir=/x\n",
        "extensions_dir requires extension_api=1",
    ),
    (
        "version=1\nextensions_dir=relative/path\n",
        "invalid extensions_dir: relative/path",
    ),
    (
        "version=1\nextensions_dir=relative\n",
        "invalid extensions_dir: relative",
    ),
    (
        "version=1\nextensions_dir=/tmp/extensions\n",
        "extensions_dir requires extension_api=1",
    ),
    (
        "version=1\nextensions_dir=$PATH\n",
        "invalid extensions_dir: $PATH",
    ),
    (
        "version=1\nextension_api=1\nextensions_dir=$HOME/path/$PATH\n",
        "invalid extensions_dir: $HOME/path/$PATH",
    ),
    (
        "version=1\nextension_api=1\nextensions_dir=~/path/~other\n",
        "invalid extensions_dir: ~/path/~other",
    ),
    (
        "version=1\nextensions_dir=~/$HOME/x\n",
        "invalid extensions_dir: ~/$HOME/x",
    ),
    (
        "dependency_provider=none\n",
        "version=1 must be the first setting",
    ),
    (
        "version=1\ndependency_provider=apt\n",
        "unsupported dependency_provider: apt",
    ),
    (
        "version=1\ndependency_provider=Shdeps\n",
        "unsupported dependency_provider: Shdeps",
    ),
    ("version=1\ndefault_profile=\n", "invalid default_profile: "),
    (
        "version=1\ndefault_profile=9abc\n",
        "invalid default_profile: 9abc",
    ),
    (
        "version=1\ndefault_profile=Bad_Name\n",
        "invalid default_profile: Bad_Name",
    ),
    (
        "version=1\ndefault_profile=base\ndefault_profile=dev\n",
        "duplicate default_profile",
    ),
    (
        "version=1\nshdeps_update_policy=sometimes\n",
        "shdeps_update_policy must be pinned or latest, found: sometimes",
    ),
    (
        "version=1\nshdeps_update_policy=invalid\n",
        "shdeps_update_policy must be pinned or latest, found: invalid",
    ),
    (
        "version=1\nshdeps_update_policy=PINNED\n",
        "shdeps_update_policy must be pinned or latest, found: PINNED",
    ),
    (
        "version=1\nshdeps_update_policy=pinned\nshdeps_update_policy=latest\n",
        "duplicate shdeps_update_policy",
    ),
];

#[test]
fn checked_in_extension_example_has_the_documented_native_configuration() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("examples/extension-dotfiles/home/.config/dot/config");
    let config = load(&Request {
        config_path: Some(&path),
        home: "/home/example",
        env_policy: None,
    })
    .expect("parse checked-in extension example");
    assert!(config.extension_api);
    assert_eq!(
        config.extensions_dir.as_deref(),
        Some("/home/example/.local/lib/dotfiles")
    );
    assert_eq!(config.provider, Provider::None);
    for relative in ["merge-hooks.d/10-example.sh", "doctor.d/10-example.sh"] {
        assert!(
            root.join("examples/extension-dotfiles/home/.local/lib/dotfiles")
                .join(relative)
                .is_file(),
            "missing extension example {relative}"
        );
    }
    let syntax =
        Command::new(dot_test_support::bash())
            .arg("-n")
            .arg(root.join(
                "examples/extension-dotfiles/home/.local/lib/dotfiles/merge-hooks.d/10-example.sh",
            ))
            .arg(root.join(
                "examples/extension-dotfiles/home/.local/lib/dotfiles/doctor.d/10-example.sh",
            ))
            .status()
            .expect("check extension example syntax");
    assert!(syntax.success());
    assert!(
        root.join("examples/minimal-dotfiles/home/.bashrc")
            .is_file(),
        "minimal example lost its one-file payload"
    );
}

#[test]
fn accepts_valid_corpus_and_env_policy_precedence() {
    let scratch = TempDir::new("config-diff").expect("scratch dir");
    for (index, (body, expected)) in VALID_CORPUS.iter().enumerate() {
        let path = scratch.write(&format!("valid-{index}"), body.as_bytes());
        require_fixture(&path, "native parse");
        assert_eq!(
            rust_parse(&path, "/home/tester", None),
            Outcome::Ok((*expected).to_string()),
            "valid case {index}: {body:?}"
        );
    }
}

#[test]
fn rejects_invalid_corpus_with_actionable_diagnostics() {
    let scratch = TempDir::new("config-diff").expect("scratch dir");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let home_str = home.to_str().expect("home utf8");
    for (index, (body, expected)) in INVALID_CORPUS.iter().enumerate() {
        let path = scratch.write(&format!("invalid-{index}"), body.as_bytes());
        require_fixture(&path, "native parse");
        assert_eq!(
            rust_parse(&path, home_str, None),
            Outcome::Err(format!("dot: config: {expected}\n")),
            "invalid case {index}: {body:?}"
        );
    }
}

#[test]
fn missing_file_uses_documented_defaults() {
    let scratch = TempDir::new("config-diff").expect("scratch dir");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let home_str = home.to_str().expect("home utf8");
    let missing = scratch.path().join("does-not-exist");
    assert_eq!(
        rust_parse(&missing, home_str, None),
        Outcome::Ok("1|||none|base|pinned\n".into())
    );

    // A value from an earlier load is not an ambient override for a later
    // missing file; each request starts from the documented defaults.
    let configured = scratch.write(
        "configured",
        b"version=1\ndefault_profile=dev\nshdeps_update_policy=latest\n",
    );
    assert_eq!(
        rust_parse(&configured, home_str, None),
        Outcome::Ok("1|||none|dev|latest\n".into())
    );
    assert_eq!(
        rust_parse(&missing, home_str, None),
        Outcome::Ok("1|||none|base|pinned\n".into())
    );
}

#[test]
fn policy_reloads_from_changed_config_unless_environment_overrides_it() {
    let scratch = TempDir::new("config-policy-reload").expect("scratch dir");
    let path = scratch.write("config", b"version=1\nshdeps_update_policy=latest\n");
    assert_eq!(
        rust_parse(&path, "/home/tester", None),
        Outcome::Ok("1|||none|base|latest\n".into())
    );

    std::fs::write(&path, b"version=1\nshdeps_update_policy=pinned\n").expect("rewrite config");
    assert_eq!(
        rust_parse(&path, "/home/tester", None),
        Outcome::Ok("1|||none|base|pinned\n".into()),
        "a config-derived policy must be re-read after a provider handoff"
    );
    assert_eq!(
        rust_parse(&path, "/home/tester", Some("latest")),
        Outcome::Ok("1|||none|base|latest\n".into()),
        "an explicit environment policy takes precedence"
    );
    assert_eq!(
        rust_parse(&path, "/home/tester", Some("invalid")),
        Outcome::Err(
            "dot: config: DOT_SHDEPS_UPDATE_POLICY must be pinned or latest, found: invalid\n"
                .into(),
        )
    );
}

#[test]
fn home_expansion_requires_home_and_handles_root_without_double_slash() {
    let scratch = TempDir::new("config-home-expansion").expect("scratch dir");
    let braced = scratch.write(
        "braced",
        b"version=1\nextension_api=1\nextensions_dir=${HOME}/extensions\n",
    );
    assert_eq!(
        rust_parse(&braced, "", None),
        Outcome::Err("dot: config: invalid extensions_dir: ${HOME}/extensions\n".into(),)
    );

    let root = scratch.write(
        "root",
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\n",
    );
    assert_eq!(
        rust_parse(&root, "/", None),
        Outcome::Ok("1|1|/extensions|none|base|pinned\n".into())
    );
}

#[test]
fn config_file_type_control_bytes_and_size_boundaries_fail_closed() {
    let scratch = TempDir::new("config-file-security").expect("scratch dir");

    let nul = scratch.write("nul", b"version=1\0dependency_provider=none\n");
    assert_eq!(
        rust_parse(&nul, "/home/tester", None),
        Outcome::Err(format!(
            "dot: config: contains control bytes: {}\n",
            nul.display()
        ),)
    );

    // The limit is inclusive: a syntactically valid 65,536-byte file loads,
    // while adding one printable byte is rejected by size before parsing.
    let mut boundary = b"version=1\n#".to_vec();
    boundary.resize(65_536, b'a');
    let exact = scratch.write("exact-limit", &boundary);
    assert_eq!(
        rust_parse(&exact, "/home/tester", None),
        Outcome::Ok("1|||none|base|pinned\n".into())
    );
    boundary.push(b'a');
    let oversized = scratch.write("over-limit", &boundary);
    assert_eq!(
        rust_parse(&oversized, "/home/tester", None),
        Outcome::Err(format!(
            "dot: config: file exceeds 65536 bytes: {}\n",
            oversized.display()
        ),)
    );

    let target = scratch.write("target", b"version=1\n");
    let link = scratch.path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink config");
    assert_eq!(
        rust_parse(&link, "/home/tester", None),
        Outcome::Err(format!(
            "dot: config: not a regular file: {}\n",
            link.display()
        ),)
    );
}

#[test]
fn relative_xdg_config_home_falls_back_to_home() {
    assert_eq!(
        xdg::base(Kind::Config, "relative", "/home/tester"),
        Ok("/home/tester/.config".to_string())
    );
}

/// Build one resolved configuration for the extensions gate.
fn rust_gate(api: bool, dir: Option<&str>) -> bool {
    extensions_enabled(&Config {
        version: 1,
        extension_api: api,
        extensions_dir: dir.map(str::to_string),
        provider: Provider::None,
        default_profile: "base".to_string(),
        shdeps_update_policy: UpdatePolicy::Pinned,
        policy_from_env: false,
    })
}

/// Configuration diagnostics retain their stable public prefix and joined detail.
#[test]
fn config_error_has_stable_prefix_and_joined_detail() {
    for (detail, expected) in [
        ("hello world", "dot: config: hello world"),
        ("a b c", "dot: config: a b c"),
        ("", "dot: config: "),
    ] {
        assert_eq!(
            config_error(detail).to_string(),
            expected,
            "detail {detail:?}"
        );
    }
}

/// Every accepted and rejected byte class is explicit, and unreadable paths
/// fail closed.
#[test]
fn control_byte_scan_handles_binary_and_unreadable_paths() {
    let scratch = TempDir::new("config-control-diff").expect("scratch dir");
    let bodies: &[&[u8]] = &[
        b"",
        b"abc\n",
        b" ~\n",
        b"\xff\xfe\x80\n",
        b"a\tb\n",
        b"a\rb\n",
        b"a\x00b",
        b"a\x7fb",
        b"a\x1fb",
    ];
    let expected = [true, true, true, true, false, false, false, false, false];
    for (index, (body, expected)) in bodies.iter().zip(expected).enumerate() {
        let path = scratch.write(&format!("control-{index}"), body);
        require_fixture(&path, "native control scan");
        assert_eq!(
            config_control_bytes(&path),
            expected,
            "case {index}: {body:?}"
        );
    }
    // Missing paths and directories cannot supply trusted configuration.
    let missing = scratch.path().join("does-not-exist");
    assert!(!config_control_bytes(&missing));
    assert!(!config_control_bytes(scratch.path()));
}

/// Only API version one plus a non-empty directory enables extensions.
#[test]
fn extensions_require_api_one_and_a_nonempty_directory() {
    let cases = [
        (false, None, false),
        (false, Some(""), false),
        (false, Some("/x"), false),
        (true, None, false),
        (true, Some(""), false),
        (true, Some("/x"), true),
    ];
    for (api, dir, expected) in cases {
        assert_eq!(rust_gate(api, dir), expected, "api {api} dir {dir:?}");
    }
}
