# Rust Port Spec (slice 1: scaffold + CLI core)

Normative compatibility contract for the first slice. Later slices extend
this file. Anything not specified here is governed by the shell
implementation plus `tests/*-test` (the oracle) — when in doubt, the shell
wins and this spec gets amended.

## 1. Version identity

- `dot version` prints exactly:
  `dot commit <rev> (config 1; extensions 1; library 1)\n`
  where `<rev>` is the 12-hex-char source revision, or the literal
  `unknown` when no revision is available (no git, thin checkout, or
  `DOT_BUILD_COMMIT` unset and unresolvable). This matches `dot_version()`
  in `lib/dot/main.sh`, including the `unknown` fallback.
- `build.rs` provides `DOT_BUILD_COMMIT` / `DOT_BUILD_VERSION` via
  `cargo:rustc-env`, resolved as: `$DOT_BUILD_COMMIT` → `$GITHUB_SHA` →
  `git rev-parse HEAD` in the source checkout → `unknown` (unlike shdeps,
  never panic: the shell contract defines `unknown`).
- `DOT_BUILD_VERSION` is `YYYYMMDD-HHMMSS-8hex`, validated
  (`8digit-6digit-8hex`); falls back to `unknown`.
- `src/version.rs` exposes `commit()`, `version()`, `description()`,
  `line()` (`"dot <version>"`) with unit tests asserting shape and
  `version[15..23] == commit[..8]` when both are known.

## 2. CLI surface (slice 1)

Commands owned by the Rust binary in this slice: `help` (default,
`-h`, `--help`), `version` (`--version`). All other commands exit via
the existing shell implementation (the binary is not yet on PATH).

- `dot help` prints the exact `dot_help` heredoc from `lib/dot/main.sh`
  (byte-identical; pinned by `tests/cli.rs` against the shell source).
- Unknown command: `dot: unknown command: <arg>\n` on stderr, exit 1.
- `version` output goes to stdout; errors to stderr; exit 0 on success.
- I/O streams are injected (`run(args, stdout, stderr) -> Result<i32>`)
  so parity tests capture text without subprocesses (shdeps `cli.rs`
  pattern).

## 3. Performance budgets (slice 1)

Measured on the reference host; enforced by `tests/perf_budget.rs`
(p95 over runs, CLI-level including process startup, following
`hive-memory` `tests/perf_budget.rs`):

| Operation | Shell baseline | Rust budget (p95) |
|---|---|---|
| `help` | ~18ms | 25ms |
| `version` (warm, git available) | ~26ms | 30ms |

- Multiplier env `DOT_PERF_BUDGET_MULTIPLIER` (float, default 1.0) exists
  for slow developer hosts only. Gate CI jobs run perf tests explicitly
  (including `#[ignore]`-gated ones) with the multiplier pinned to 1.
- Budgets cover warm and cold paths; update-level budgets (slice 2+)
  assert improvement against the recorded shell ceiling, not just an
  absolute ceiling.
- Heavy/loop-heavy budgets (e.g. full `update` on fixtures) are
  `#[ignore]`-gated and run explicitly in CI, same as hive-memory.
- Budgets are regression gates, not goals: the port must beat them by a
  wide margin on the reference host; a budget failure blocks the slice.

## 4. Crate layout (slice 1)

`Cargo.toml` (`edition 2024`, `rust-version 1.85`, `[lints.rust]
warnings = "deny"`, lib+bin), `build.rs`, `src/lib.rs`, `src/main.rs`
(thin adapter: exit-code passthrough), `src/errors.rs` (hand-rolled,
no anyhow/thiserror in lib), `src/version.rs`, `src/cli.rs`,
`src/test_support.rs`, `tests/cli.rs`, `tests/perf_budget.rs`.

## 5. Forward contracts (owned by later slices, recorded here)

Full command table (`lib/dot/commands.sh`, `lib/dot/main.sh`):

| Command | Behavior | Exit codes |
|---|---|---|
| `update`, `pull` (alias) | update lock + `_dot_update`; flags `--cron --quiet -f/--force -v/--verbose`, rest to `git pull` | 0, 2 (config load fail), 75 (lock busy) |
| `fetch` | overlay-resolve `fetch` + per-repo `git fetch` passthrough | 0/1 |
| `push` | resolve `inspect` + per-repo `git push`; base failure hard-fails, overlay warns+continues | 0/1 |
| `status`, `diff` | resolve `inspect` + per-repo passthrough | 0/1 |
| `cron` | `crontab -l` or `  no crontab installed` | 0 |
| `doctor` | resolve tolerated + `_dot_doctor` | 0/1 |
| `test` | resolve + `dot_test_command` (`-s -v -j N --list [names]`) | runner codes |
| `init` | lock (except `--status/--help/-h`) + `dot_init_command` | 0/1/2 (unknown `--*`), 75 |
| unknown | `dot: unknown command: %s` on stderr | 1 |

Environment (precedence: process env wins; captured at load): `DOT_BASH`,
`DOT_FORCE`/`SHDEPS_FORCE`, `DOT_QUIET`/`SHDEPS_QUIET`,
`DOT_VERBOSE`/`SHDEPS_LOG_LEVEL=2`, `DOT_UPDATE_JOBS`, `DOT_MERGE_JOBS`,
`DOT_TEST_*`, `DOT_UI_*`, `DOT_OVERLAY_*`, `DOT_PROFILE_*`,
`DOT_REEXEC_*`, `DOT_CLEANUP_*`, `DOT_INIT_SKIP_PROVIDER`,
`DOT_SHDEPS_*`, `SHDEPS_JOBS`, `XDG_*` (relative = unset),
`NO_COLOR`, `PATH`, `REPLY` (cleared on entry).

State formats (golden tests required in owning slice): lock `owner`
(`pid\tstart\ttoken`, mode 600); overlay record
`name|path|url|conf|optional|sync` (missing sync = `git`); managed-block
markers (`# <marker> begin` / `# DO NOT EDIT...` / `# source: <path>` /
`# <marker> end`); hook identity `^([0-9]+[-_])?([a-z][a-z0-9-]*)$`
(`*.serial.sh` = barrier); provider reexec checkpoint
(`cgraf78 dot provider reexec checkpoint v1`, `before=/after=` hex).

## 6. Non-contract (explicitly out of slice 1)

Config parsing, XDG resolution, update pipeline, extension workers,
providers, doctor/test/init commands, `bin/dot` cutover, release
workflow, man pages, shell completions. Each gets its own spec section
in a later slice; none of the shell behavior for those paths may change
as a side effect of this slice (shell suite must stay 28/28 green).
