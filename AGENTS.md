# AGENTS.md

## About

`dot` is a declarative dotfiles manager. The Bash engine under `lib/`
is the current behavior owner; a Rust port is in progress
(`docs/rust-port-plan.md`, `docs/rust-port-spec.md`). Until a slice
cuts over, the Rust crate must not change any shell behavior.

## Architecture

- `src/lib.rs` owns the Rust implementation (one module per domain).
- `src/main.rs` is a thin adapter: exit-code passthrough only.
- `bin/dot` remains the entry point; the Rust binary is not on PATH.
- `build.rs` resolves `DOT_BUILD_COMMIT`/`DOT_BUILD_VERSION`; the
  `unknown` fallback is contract (`dot version` prints it, never fails).
- Public shell API boundaries (`lib/dot/public/*`, `hook-api-v1.tsv`,
  `doctor-api-v1.tsv`, `test-api-v1.tsv`) are compatibility constraints.

## Testing

- Rust: `cargo test --locked` (unit + integration + perf budgets).
- Perf-heavy: `cargo test --locked -- --ignored` (no ignored tests in
  slice 1; update harness joins in slice 2+), gate jobs pin
  `DOT_PERF_BUDGET_MULTIPLIER=1`.
- Shell oracle (must stay 28/28 green): `bash tests/run`.
- Lints: `cargo clippy --locked --all-targets --all-features -- -D warnings`
  (`[lints.rust] warnings = "deny"` covers rustc lints locally; Clippy
  itself is enforced by the CI flag).
- ShellCheck inventory: `.github/shellcheck-files.txt` (do not regress).

## Rules for port slices

- Every behavior change needs the shell suite green AND a new Rust test.
- New parallelism needs a stress test, not just parity tests.
- Byte-exact outputs modulo stated exclusions (timing fields, spinner
  frames, already-parallel replay order — see plan).
- Update `docs/rust-port-spec.md` forward contracts when a slice claims
  new surface; never drift the TSVs without an intentional change.
