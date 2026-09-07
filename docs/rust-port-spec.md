# Rust Port Compatibility Record

This records the compatibility contracts used during the incremental Rust
port. The native implementation and its tests now own the behavior. Historical
references to the removed shell engine explain the source of a contract; they
are not a second runtime or a test oracle.

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
- `DOT_BUILD_VERSION` accepts any non-empty `$DOT_BUILD_VERSION`, otherwise
  `unknown`. Release builds supply the shared validated
  `YYYYMMDD-HHMMSS-8hex` identity.
- `src/version.rs` exposes `COMMIT` / `SHORT_COMMIT` / `VERSION`
  consts plus `version_line()` (exact `dot version` text) and
  `description()`, with unit tests asserting the revision is `unknown`
  or 12 hex chars.

## 2. CLI surface (historical slice-1 contract)

The first migration slice covered `help` (default, `-h`, `--help`) and
`version` (`--version`). Subsequent slices ported the full command table in
section 5 and cut the installed entry point over to the Rust binary.

- Help and version bypass configuration loading and remain available for
  diagnosis. Operational and unknown commands load configuration first; an
  unloadable configuration exits 2 before their dispatch.
- `dot help` prints the exact `dot_help` heredoc from `lib/dot/main.sh`
  (byte-identical; pinned by `tests/cli.rs` against the shell source).
- Unknown command: `dot: unknown command: <arg>\n` on stderr, exit 1.
- `version` output goes to stdout; errors to stderr; exit 0 on success.
- I/O streams are injected (`run(args, stdout, stderr) -> i32`)
  so parity tests capture text without subprocesses (shdeps `cli.rs`
  pattern). (`Result` is reserved for fallible engine operations in
  later migration work; slice-1 dispatch was infallible by construction.)

## 3. Performance budgets (historical slice-1 baseline)

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
- Budgets cover warm and cold startup paths. The completed update-level
  benchmark and measured comparison are recorded in
  `docs/rust-port-performance.md`.
- Heavy/loop-heavy budgets (e.g. full `update` on fixtures) are
  `#[ignore]`-gated and run explicitly in CI, same as hive-memory.
- Budgets are regression gates, not goals: the port must beat them by a
  wide margin on the reference host; a budget failure blocks the slice.

## 4. Initial crate layout (historical)

`Cargo.toml` (`edition 2024`, `rust-version 1.85`, `[lints.rust]
warnings = "deny"`, lib+bin), `build.rs`, `src/lib.rs`, `src/main.rs`
(thin adapter: exit-code passthrough), `src/errors.rs` (hand-rolled,
no anyhow/thiserror in lib), `src/version.rs`, `src/cli.rs`,
`src/test_support.rs`, `tests/cli.rs`, `tests/perf_budget.rs`.

## 5. Full command contracts

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

Migration surface (each row was ported with shell-vs-Rust differential tests
before native cutover):

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
| `merges` | `merges.sh` (pure half) | label derivation; serial detection; job counts (verbatim counts, signed warning math, `getconf`/`sysctl` chain); summaries; `%03d` result prefixes; batch/capture/run stay shell until the progress-UI/worker/context slices |
| `profiles` | `profile-format.sh` + `profiles.sh` (load/select) | definition parse; include expansion with cycle detection; selector matching by specificity; default resolution; `od`-star fail-closed replication; sorted validation order (shell hash order only affects multi-error precedence) |
| `overlay_context` | `overlay-context.sh` | NUL-framed one-use contexts; field/path/record/matrix validators; ownership-gated file safety; `od` repeat-marker fail-closed; cross-engine frame compatibility both directions |
| `overlays` | `overlays.sh` (+ `repos/config.sh` checkout match) | descriptor parse; strict/legacy discovery with selection echo; name derivation mirroring bash `=~` `$` newline anchoring; glob-exact `*.conf` filter (dotfiles skipped); `mapfile`-empty origin semantics; warning-text discovery errors (never announced) |
| `version::LIBRARY_API` | `public/api-version.sh` | `DOT_LIBRARY_API=1` pinned on both sides |

## 6. Native test supervisor

The Rust `test` command owns arguments, inspect-mode resolution, source-home
authority, trusted suite discovery, scheduling, result classification, output,
timeouts and cancellation. It never loads `lib/dot/test.sh` or its stage files.
The removed private shell files served as the migration oracle before cutover.
The public `test-reporter-v1` and `test-timeout-v1` interfaces remain available
for external suites. The native scheduler does not execute the timeout helper.

Source authority requires both the configured base Git common directory and
Git's registration of the exact source worktree. The actual invocation HOME is
the trust anchor; caller-supplied host metadata cannot replace it. Source-path
comparisons retain Unix bytes. Shared client identity selection belongs to
`repos_base`, and executable lookup belongs to the immutable `Runtime`.

Each suite receives closed stdin, private cache/state/temp paths, and its own
session. The supervisor observes child exit without reaping the leader until
owned descendants have received TERM and, if necessary, KILL. Cancelling a
worker wave uses one shared grace deadline. Independent CLI/embedded
invocations run in separate processes and cannot share signal guards or roots.
`app::run_direct` remains a single-process entry, not a concurrent embedding API.

Coordinator-created result/output file handles remain authoritative throughout
execution. Replacing their directory entries cannot redirect readers to FIFOs
or foreign files. Suites write the supplied result file in place, as the public
reporter does. Sequential output is streamed in bounded snapshots; parallel
replay follows selected-suite order. Wall-clock marks and already-parallel
completion order retain the existing parity exclusions.

The only added dependency is `libc`, for POSIX operations absent from std:
signal handling/delivery, session identity, and `waitid(WNOWAIT)`. Unsafe calls
are centralized in `cleanup`; std owns Command/Child, file I/O and reaping.

## 7. Slice-1 exclusions (historical record)

Config parsing, XDG resolution, update pipeline, extension workers,
providers, doctor/test/init commands, `bin/dot` cutover, release
workflow, man pages, and shell completions were deliberately outside the first
increment. Later increments brought the CLI and engine paths into the native
binary before the private shell implementation was removed.
