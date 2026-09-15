//! Native contracts for quiet gating, stream routing, and uncolored bytes.

fn rust_log(
    function: &str,
    text: &str,
    quiet: Option<&str>,
    no_color: Option<&str>,
) -> (i32, String, String) {
    // Piped stdout on both sides: colors disabled, like the shell.
    let log = dot::log::Log::from_env(false, no_color, quiet);
    let mut out = Vec::new();
    let mut err = Vec::new();
    match function {
        "_log" => log.log(&mut out, text),
        "_header" => log.header(&mut out, text),
        "_log_header" => log.log_header(&mut out, text),
        "_log_ok" => log.ok(&mut out, text),
        "_log_dim" => log.dim(&mut out, text),
        "_warn" => log.warn(&mut err, text),
        _ => panic!("unknown function {function}"),
    }
    (
        0,
        String::from_utf8(out).expect("utf8"),
        String::from_utf8(err).expect("utf8"),
    )
}

#[test]
fn log_matrix_has_stable_streams_and_bytes() {
    let functions = [
        "_log",
        "_header",
        "_log_header",
        "_log_ok",
        "_log_dim",
        "_warn",
    ];
    // Decimal spellings both engines agree on (exotic bash-arithmetic
    // forms like `0x1` are out of contract; see `is_quiet` docs).
    let quiets: &[Option<&str>] = &[None, Some(""), Some("0"), Some("1"), Some("2")];
    let no_colors: &[Option<&str>] = &[None, Some(""), Some("1")];
    // Two-word message locks the shell `echo "$@"` join contract
    // against the pre-joined Rust `&str`.
    for function in functions {
        for quiet in quiets {
            for no_color in no_colors {
                let rust = rust_log(function, "hello world", *quiet, *no_color);
                let suppressed = *quiet == Some("1") && !matches!(function, "_header" | "_warn");
                let expected = if suppressed { "" } else { "hello world\n" };
                let want = if function == "_warn" {
                    (0, String::new(), expected.to_string())
                } else {
                    (0, expected.to_string(), String::new())
                };
                assert_eq!(
                    rust, want,
                    "{function} quiet={quiet:?} no_color={no_color:?}"
                );
            }
        }
    }
}
