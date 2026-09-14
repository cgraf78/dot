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
- `build.rs` provides `DOT_BUILD_COMMIT` / `DOT_BUILD_SHORT_COMMIT` /
  `DOT_BUILD_VERSION` via `cargo:rustc-env`, resolved as:
  `$DOT_BUILD_COMMIT` → `$GITHUB_SHA` → `git rev-parse HEAD` walking up
  from the manifest dir → `unknown` (unlike shdeps, never panic: the
  shell contract defines `unknown`). The short commit is the lowercased
  first 12 hex chars, else `unknown`.
- `DOT_BUILD_VERSION` accepts any non-empty `$DOT_BUILD_VERSION`,
  else `unknown`. The shared `YYYYMMDD-HHMMSS-8hex` scheme and its
  validation arrive with the release workflow in a later slice.
- `src/version.rs` exposes `COMMIT` / `SHORT_COMMIT` / `VERSION`
  consts plus `version_line()` (exact `dot version` text) and
  `description()`, with unit tests asserting the revision is `unknown`
  or 12 hex chars.

## 2. CLI surface (slice 1)

Commands owned by the Rust binary in this slice: `help` (default,
`-h`, `--help`), `version` (`--version`). The binary is not yet on
PATH, so the shell remains the entry point; direct invocations of
unported commands (`update`, `status`, …) yield the shell's
`unknown command` / exit 1 until their owning slice lands.

- Unknown command exits 1 **when config loads** (the shell runs
  `dot_config_load || exit 2` before dispatch, so an unloadable config
  exits 2 for ANY command — specified in forward contracts, tested in
  slice 2 with the config parser).
- `dot help` prints the exact `dot_help` heredoc from `lib/dot/main.sh`
  (byte-identical; pinned by `tests/cli.rs` against the shell source).
- Unknown command: `dot: unknown command: <arg>\n` on stderr, exit 1.
- `version` output goes to stdout; errors to stderr; exit 0 on success.
- I/O streams are injected (`run(args, stdout, stderr) -> i32`)
  so parity tests capture text without subprocesses (shdeps `cli.rs`
  pattern). (`Result` is reserved for fallible engine operations in
  later slices; slice-1 dispatch is infallible by construction.)

## 3. Performance budgets (slice 1)

Measured on the reference host; enforced by `tests/perf_budget.rs`
(p95 over runs, CLI-level including process startup, following
`hive-memory` `tests/perf_budget.rs`):

| Operation | Shell baseline (warm) | Rust budget (p95) | Rust expected |
|---|---|---|---|
| `help` | ~18ms (parse+probes) | 25ms | ~2-5ms |
| `version` | ~26ms (incl. one `git rev-parse` fork; Rust bakes the revision, no fork) | 30ms | ~2-5ms |

Budgets are CI-variance ceilings, not targets: the port must beat them
by an order of magnitude on the reference host; a change that merely
squeaks under budget without improving on the shell has failed the
point of the port even if the gate is green.

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
| unknown | `dot: unknown command: %s` on stderr | 1 (2 if config unloadable — config load precedes dispatch) |

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

Claimed surface (each row ported with shell-vs-Rust differential
tests; the binary is still not on PATH and no shell behavior changed):

| Rust module | Shell source | Parity notes |
|---|---|---|
| `glob` | `case`-pattern semantics (shared) | byte-oriented C-locale matcher; `\|` from variables is literal; descending ranges void; post-void dash stages shadowed; pinned to bash 5.x (`DOT_BASH`); macOS system bash 3.2 trailing-`\` corner differs, not a supported engine runtime |
| `platform` | `platform.sh` | `command -v` needs no exec bit; `[[ "" -eq 0 ]]` id coercion replicated in `require_sudo`; spec sides both literal (quoted RHS), first line only (`read -a`) |
| `reserved` | `reserved.sh` | roots inventory compared line-for-line; ancestor-swallowing candidate rule; leaf symlinks resolve `realpath`-style (dangling included) |
| `families` | `families.sh` | byte-ordered stream incl. non-UTF8 names; patterns filter before `.replace` selection |
| `constants` | `constants.sh` | `${VAR:-0}` substitutes on empty too |
| `temp` | `temp.sh` | generation tokens (verbatim string compares, trailing-delimiter quirk); prepare/quarantine/commit/remove with shell-identical unwinds; `mv` via the same probed binary (BSD nesting recovery); git-sha digests under the sanitized binding; umask read from the process; sorted tree walk (deterministic; success end-state order-free) |
| `merge_block` | `merge-block.sh` | modeline strip + shell-whitespace trim; every `sed`-range strip (same-line ranges stay open); family strips; squeeze-join-finalize with digest-skipped 600 publish; re-merge is mtime-identical |
| `merge_hooks` | `merge-hooks.sh` | XDG hooks root; family stream/markers/relpaths; narrow `${HOME}`/`$HOME`/`~` expansion; text writes; `jq` layer with stderr-forwarded warnings and corrupt rebuilds |
| `version::LIBRARY_API` | `public/api-version.sh` | `DOT_LIBRARY_API=1` pinned on both sides |

## 6. Non-contract (explicitly out of slice 1)

Config parsing, XDG resolution, update pipeline, extension workers,
providers, doctor/test/init commands, `bin/dot` cutover, release
workflow, man pages, shell completions. Each gets its own spec section
in a later slice; none of the shell behavior for those paths may change
as a side effect of this slice (shell suite must stay 28/28 green).
