# AGENTS.md

## About

`dot` is a declarative dotfiles manager. The native Rust CLI and engine own
product behavior. Shell remains only at explicit public extension, bootstrap,
packaging, and test-harness boundaries.

## Architecture

- `src/lib.rs` owns the Rust implementation (one module per domain).
- `src/main.rs` is a thin adapter: exit-code passthrough only.
- `bin/dot` is the development adapter for the native binary; releases install
  the compiled `dot` executable directly.
- `build.rs` resolves `DOT_BUILD_COMMIT`/`DOT_BUILD_VERSION`; the
  `unknown` fallback is contract (`dot version` prints it, never fails).
- Public shell API boundaries (`lib/dot/public/*`, `hook-api-v1.tsv`,
  `doctor-api-v1.tsv`, `test-api-v1.tsv`) are compatibility constraints.

## Testing

- Rust: `cargo test --locked` (unit, integration, and deterministic performance
  policy tests; no wall-clock gates).
- Performance: `scripts/benchmark-port.sh` (Linux-only ignored release-mode
  paired gate over clean committed source trees).
- Public-boundary shell acceptance: `bash tests/run`.
- Lints: `cargo clippy --locked --all-targets --all-features -- -D warnings`
  (`[lints.rust] warnings = "deny"` covers rustc lints locally; Clippy
  itself is enforced by the CI flag).
- Standalone Rust: `rustfmt --check support/performance-command-supervisor.rs
  tests/support/performance-supervisor-fixture.rs` (these files are compiled
  directly and are outside Cargo's module graph).
- ShellCheck inventory: `.github/shellcheck-files.txt` (do not regress).

## Rules for native changes

- Every behavior change needs a focused native test and the applicable
  public-boundary acceptance suites green.
- New parallelism needs a stress test, not just parity tests.
- Byte-exact outputs modulo stated exclusions (timing fields, spinner
  frames, already-parallel replay order — see plan).
- Keep `docs/rust-port-spec.md` as a historical compatibility record; never
  drift the TSVs without an intentional change.
