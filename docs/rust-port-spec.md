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

## 3. Performance budgets

`tests/perf_budget.rs` owns deterministic policy tests. The ignored
`tests/perf_update.rs` harness is run only in Cargo's release profile by the
dedicated Ubuntu performance job; ordinary debug and portability jobs do not
make wall-clock assertions.

| Operation | Samples | Rust requirement |
|---|---:|---|
| first `help` spawn | 1 before warm-up | no more than 100ms |
| warm `help` | median and p95 of 30 paired runs after 10 warm-ups | median no more than 75% of Bash; p95 no more than 24.075ms |
| warm `version` | median and p95 of 30 paired runs after 10 warm-ups | median no more than 75% of Bash; p95 no more than 23.85ms |
| clean base-only update | median and p95 of 30 paired runs | median no more than 75% of Bash; p95 no more than 4s |
| clean disjoint three-overlay update | median and p95 of 30 paired runs | median no more than 75% of Bash; p95 no more than 1.6025s |
| dirty disjoint three-overlay update | median and p95 of 30 independently dirtied paired runs | median no more than 75% of Bash; p95 no more than 2.01s |
| profile/provider/hooks/collision update | median and p95 of 30 paired runs | median no more than 75% of Bash; p95 no more than 12s |
| failing pre-sync update | median and p95 of 30 paired runs | median no more than 75% of Bash; p95 no more than 4s |

The historical commit is centralized in
`support/performance-baseline-v1.tsv`; one shared policy table supplies the
harness and deterministic tests with workload order, sample counts, warm-ups,
and acceptance limits. Each gate run emits anonymized calibration ratios and
headroom bound to its candidate, baseline, provider, toolchain, and evidence
hashes. The gate covers every workload required by the completion design, and
every accepted update sample has already passed output, expected-payload, and
normalized filesystem-state checks. Raw nanosecond samples, path-sanitized
environment metadata, calibration, and the atomic completion manifest are
uploaded as an exact six-file set only after independent validation succeeds;
rejected partial evidence is not published.
The exact methodology and its remaining coverage boundary are recorded in
`docs/rust-port-performance.md`.

The measured provider is always the revision pinned by the current candidate.
Its ABI must match the historical shell lock, but the old and current lock
revisions are intentionally allowed to differ so later compatible provider
fixes do not invalidate the immutable shell baseline.

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
| `update`, `pull` (alias) | update lock + `_dot_update`; flags `--cron --quiet -f/--force -v/--verbose`, rest to `git pull` | 0, 1, 2 (config load fail), 75 (lock busy), or 129/130/131/143 after HUP/INT/QUIT/TERM |
| `fetch` | overlay-resolve `fetch` + per-repo `git fetch` passthrough | 0/1, or 129/130/131/143 after HUP/INT/QUIT/TERM |
| `push` | resolve `inspect` + per-repo `git push`; base failure hard-fails, overlay warns+continues | 0/1, or 129/130/131/143 after HUP/INT/QUIT/TERM |
| `status`, `diff` | resolve `inspect` + per-repo passthrough | 0/1, or 129/130/131/143 after HUP/INT/QUIT/TERM |
| `cron` | `crontab -l` or `  no crontab installed` | 0, or 129/130/131/143 after HUP/INT/QUIT/TERM |
| `doctor` | resolve tolerated + `_dot_doctor` | 0/1, or 129/130/131/143 after HUP/INT/QUIT/TERM |
| `test` | resolve + `dot_test_command` (`-s -v -j N --list [names]`) | runner codes, or 129/130/131/143 after HUP/INT/QUIT/TERM |
| `init` | lock (except `--status/--help/-h`) + `dot_init_command` | 0/1/2 (unknown `--*`), 75, or 129/130/131/143 after HUP/INT/QUIT/TERM |
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

The native Shdeps relay preserves byte order within provider stdout and within
provider stderr. As in the shell implementation's JSONL FIFO plus inherited
stderr, it does not define a total order between writes to the two independent
descriptors. After cancellation, bytes already relayed remain exactly once;
queued, unrelayed bytes are best-effort and may be discarded so a provider
cannot defeat the bounded shutdown contract through output backpressure. No
post-cancellation JSONL event is interpreted or acknowledged.

Provider selection requires the locked wrapper ABI and both independent
behavioral capabilities: `owned-subprocess-cancellation-v1` and
`prompt-fifo-reader-before-event-v1`. The former makes provider statuses 129,
130, 131, and 143 trusted reports that owned descendants stopped after HUP,
INT, QUIT, or TERM. The latter proves the provider opens and retains the FIFO
reader before publishing a prompt, so Dot can acknowledge through a fresh
nonblocking writer without a post-exit race. Missing either capability forces
one reviewed bootstrap refresh; a second mismatch is rejected. Dot propagates
a trusted cancellation status and skips all later update stages. Other nonzero
provider statuses remain ordinary failures. `dot doctor` checks both
capabilities, and every doctor probe runs under Dot's signal owner so
cancellation reaps its isolated session.

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

On Linux and Android, subreaper adoption, retained start identities, and
pidfds extend cancellation/timeout ownership across ordinary `setsid`, closed
descriptor, and cleared-environment descendants. Other Unix platforms can
prove ownership through the retained session leader, observed process
topology, and cooperative lifetime/control descriptors; a descendant that
deliberately discards all three before its parent exits is outside portable
process-only authority. Such an observed escape reports cleanup-incomplete
(125) instead of signaling an unpinned PID. The native scheduler preserves the
historical successful-command behavior for a deliberately detached process
that closes every ownership channel, while still cleaning every same-group
descendant before releasing the retained leader. The standalone public timeout
helper has the stronger Linux subreaper contract and cleans an attributable
detached descendant even after an otherwise successful leader exit.

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
