# Rust Port Plan

This document plans the port of `dot` from Bash to Rust while preserving
behavior exactly. It mirrors the approach that succeeded for `shdeps`
(see `shdeps` `docs/rust-port-plan.md`): the existing shell test suite
(`bash tests/run`, 28 suites, 28/28 green at `186b629` on 2026-09-02) is
the parity oracle, and no slice lands unless that suite plus the new
Rust tests pass.

Reviewed by two fresh-eyes passes (correctness/completeness,
performance/test-strategy); their accepted findings are incorporated
below. Items marked **[REVIEW]** changed because of review.

## Goals

- Preserve the `dot` CLI interface exactly (commands, flags, help text,
  exit codes, stdout/stderr split). Full table in the spec appendix.
- Preserve the config format, overlay descriptor grammar, profile
  selectors, state files, lock files, manifest, and managed-block formats
  with golden tests per format.
- Preserve the public shell APIs (`lib/dot/public/*`, hook/doctor/test
  extension APIs) and their versioned TSV contracts.
- Preserve the launcher contract (`bin/dot` trampoline, `DOT_BASH`
  selection, `DOT_REEXEC_*` guard) — cutover needs its own launcher spec.
- Substantially improve performance, priority `dot update`, measured by
  an update-level fixture harness (not just startup).
- Provide a first-class Rust library crate (`dot`) reused by the CLI.
- Keep and extend test coverage: Rust unit + integration + perf-budget +
  stress tests, with the shell suite as the no-regressions gate.
- Follow the `shdeps`/`termnav`/`grafhome-ca`/`hive-memory` conventions:
  `Cargo.toml` shape, `build.rs` version scheme, `errors.rs`, hand-rolled
  `cli.rs`, `test_support.rs`, `tests/perf_budget.rs`, CI via
  `cgraf78/actions` reusable workflows, `.github/cgraf78-actions.lock`.

## Non-Goals

- Do not change the config language, descriptor grammar, CLI surface, or
  any on-disk format during the port.
- Do not require users to migrate existing checkouts or state files.
- Do not require a Rust toolchain on machines that only run `dot`
  (ship prebuilt binaries; keep `bin/dot` working until cutover).
- Do not remove or break the sourceable Bash API until the Rust
  implementation owns every behavior and the shell suite runs against it.
- Do not re-parallelize the already-parallel phases (`_pull_overlays`,
  `_run_merge_hook_batch`) with different semantics; preserve their
  ordering, caps (`DOT_UPDATE_JOBS`, `DOT_MERGE_JOBS`), and serial
  fallbacks.
- **[REVIEW]** Do not change cross-phase ordering (base-before-overlays,
  sequential pre-sync, serial link pass) without an explicit design
  decision, spec section, and version bump. Cheaper safe wins come first.

## Baseline measurements (reference host nas, 2026-09-02, method note)

- `dot help` (n=5, warm): ~18ms/call — pure interpreter+parse cost.
- `dot version` (n=5, warm): ~26ms/call — includes one `git rev-parse`
  fork (~half the cost is the fork, not the parse).
- `bash tests/run`: 28/28 pass; slowest `init-test` 96s,
  `ownership-transfer-test` 62s, `repos-test` 57s (parallel scheduler).
- **[REVIEW]** These are provisional (n=5, warm only, no host spec).
  Slice 2 replaces them with p95-over-30 warm+cold numbers and adds the
  first `dot update` fixture measurement (N overlays x M files,
  network-isolated). No "X% faster" claim may cite the n=5 numbers.

## Safe-optimization rules

Every optimization applied during the port must satisfy all four:

1. **Byte-exact outputs, modulo stated exclusions**: stdout/stderr text,
   exit codes, and every written file are identical to the shell
   implementation for the same inputs. **Excluded**: wall-clock timing
   fields, spinner frames, and interleave order of already-parallel
   worker output (which the shell replays in declaration order — that
   replay order IS the contract). **[REVIEW]**
2. **Covered twice**: the shell suite passes unchanged AND a new Rust
   test pins the behavior — unit for pure logic, integration for CLI
   text, perf-budget for latency, **stress for new concurrency**
   (race/flakiness/fd-leak/signal paths the deterministic suite cannot
   hit). **[REVIEW]**
3. **No contract drift**: versioned TSVs and `docs/*.md` update in the
   same commit if — and only if — an intentional contract change lands.
4. **Listed with rationale and tests**: each optimization names both;
   an item without them is not approved.

Pre-approved optimizations (each names its tests):

- Eliminate per-invocation Bash parse + resolver probes (Rust startup).
  Tests: `tests/perf_budget.rs` startup budgets; `tests/cli.rs` parity.
- In-process clock/JSON for UI ticks and progress events (no `date`,
  `jq`/`sed` forks). Tests: output-parity tests on tick/progress text;
  perf budget on a tick-heavy fixture.
- Batch per-file git ops over pathspec batches. Tests: existing
  `batch-verification-test` stays green; Rust golden tests on the
  exact argv sequences issued.
- Cache `dot_config_load` keyed on config-file mtime+size plus an env
  epoch (NOT base HEAD — the loader reads the file and env, and
  HEAD-keying both over- and under-invalidates). **[REVIEW]** Tests:
  mtime-change, same-mtime rewrite, env-change unit tests.
- Parallelize read-only `status`/`diff` fan-out over repos (no lock,
  no writes; ordering = declaration order replay). Tests: order pins.
  `fetch`/`push` stay serial pending their own safety cases. **[REVIEW]**

Explicitly NOT pre-approved (need design decisions first): overlapping
base fetch with overlay fetches (policy-resolution ordering
`update.sh:209-218`); mtime-caching `_discover_overlays` (re-scans are
intentional post-pull re-resolution); parallel `_link_overlays`
(conflict-resolution semantics unwritten); changing pre-sync
sequentiality. **[REVIEW]**

## Slices (each a green PR, no merges) **[REVIEW: restructured]**

1. **Scaffold** (this branch): crate layout, `build.rs` version scheme,
   `errors`, `cli` (`help`/`version`/unknown-command parity), unit +
   integration + perf-budget tests, CI rust job, `AGENTS.md`, plan/spec
   docs. Binary does not replace `bin/dot` yet.
2. **Foundations + update harness**: strict config grammar + rejection
   table, XDG resolution, `temp.sh`/`resources.sh` semantics (traps,
   FDs, umask, transactions), `public/` API parity, AND the end-to-end
   update fixture harness with shell-measured budgets (fails nothing
   yet — it records the ceiling the port must beat).
3. **Update pipeline (minus provider)**: lock, repo sync, converge,
   link, finalize — module by module, provider stages explicitly
   excluded (shell provider code keeps running those stages).
4. **Providers + workers**: shdeps provider parity, hook
   discovery/ordering, worker isolation + trust checks (`extension-trust.sh`,
   worker baseline/sanitization), merges batching, pre-sync as-is.
5. **Commands**: `init` (own slice: transaction dirs, host-git binding,
   backup/identity/nonce), then `doctor`+`test`, then `fetch/push/status/diff/cron`.
6. **Cutover**: launcher spec first (who resolves Bash, who checks
   revision, single lock/manifest owner — no split-brain fallback),
   then `bin/dot` delegation.

Module-to-slice assignment (every file owned): 1: docs/CI/scaffold;
2: `config.sh`, `constants.sh`, `temp.sh`, `resources.sh`, `log.sh`,
`platform.sh`, `public/*`, `reserved.sh`, `families.sh`; 3: `update.sh`,
`update-lock.sh`, `merges.sh` (batching only), `merge-block.sh`,
`merge-hooks.sh`, `overlays.sh`, `overlay-context.sh`, `profiles.sh`,
`profile-format.sh`, `profile-lifecycle.sh`, `repos/*`, `run.sh`,
`runtime.sh`, `progress-ui.sh`, `pre-sync.sh` (as-is); 4: `providers/*`,
`extension-*.sh`, `hook-api.sh`, `doctor*.sh`, `doctor/*`; 5: `init*.sh`,
`test*.sh`, `test/*`, `commands.sh`, `main.sh` dispatch, `init-client.sh`
(own PR inside the slice); 6: `bin/dot`, `support/*`, `install.sh`.

## Perf-test invocation (pinned) **[REVIEW]**

- `tests/perf_budget.rs` follows hive-memory (CLI wall-clock, p95,
  heavy tests `#[ignore]`-gated).
- Gate CI runs the shared default `cargo test --locked` (all slice-1
  perf tests run un-ignored). `#[ignore]`-gated heavy tests, starting
  with the slice-2 update harness, get an explicit `test-command`
  override running them with `DOT_PERF_BUDGET_MULTIPLIER=1`; the
  multiplier exists for slow developer hosts only and gate jobs must
  not raise it.
- Budgets cover warm AND cold paths; update budgets assert against the
  slice-2 fixture ceiling (improvement required, not just a ceiling).

## Deployment + migration (fleet rollout)

Two corrected premises (verified live on nas, 2026-09-02):

- Fleet cron runs `dot update --cron`, NOT `-f` (`*/30 * * * * dot
  update --cron && shdeps prune -y`). `--cron` = quiet + exit 0 when
  dirty; `-f` only sets `DOT_FORCE`/`SHDEPS_FORCE` (best-effort
  re-download), it is not the deploy mechanism.
- `dot update` NEVER touches the engine today: it syncs base +
  overlays only. Engine upgrades require re-running `install.sh`
  out-of-band; update merely *survives* engine changes via re-exec
  (`DOT_REEXEC_ONCE`, expected-revision guard, one-generation
  checkpoint). nas itself uses a dev-checkout symlink and does not
  self-update.

Therefore seamless fleet deployment needs a NEW engine self-update
stage (design decision, requires explicit approval before slice 6):
new first stage in `_dot_update` (before repo sync) fetching a pinned
release asset via a `support/dot-release.lock` (version/revision +
sha256, mirroring `shdeps.lock` + `verify-shdeps-lock`), staged
`mv -n` publish keeping the last working binary until the new one
passes its `__api version` probe, then the existing re-exec path
(generalized from git-HEAD to binary version). `DOT_FORCE=1` maps to
force re-download, exactly like `SHDEPS_FORCE`.

Migration cases (each needs a passing test before cutover): fresh
install (curl+shasum only); managed FF update; dirty/detached/foreign
checkout fails closed; dev-checkout symlink hosts never "upgraded";
in-flight update during upgrade (single re-exec, double-change
checkpoint); lock held (exit 75, silent cron); macOS bash3.2
trampoline + `DOT_BASH` strict override preserved; rollback on bad
binary (never delete last working binary first); adapter preservation
(byte-identical `~/.local/bin/dot`); offline/air-gapped (warn, never
brick — links stay intact).

Live end-to-end gates (run on nas hardware, isolated HOME — never the
live HOME — before any cutover PR): full `init` of a fixture client
followed by `update` converging base+overlays+hooks against local
fixture remotes; cron-mode run (`--cron`) asserting silence and exit 0
on dirty/clean; engine self-update run (old binary → new binary
mid-update, asserting single re-exec and identical final links);
rollback run (bad binary published, asserting previous binary restored
and links untouched). Each gate compares the final HOME tree
byte-for-byte against the shell implementation's result on the same
fixture. No cutover slice merges while any gate diverges.

## Reference inputs

- `bin/dot`, `lib/dot/*.sh`, `lib/dot/{repos,providers,doctor,test}/`,
  `support/`, `install.sh`: current implementation.
- `tests/*-test` + `tests/run`: parity oracle (must stay green).
- `docs/*.md`, versioned `*-v1.tsv` files: normative contracts.
- `shdeps` `docs/rust-port-plan.md` / `docs/rust-port-spec.md`: method.
- `hive-memory` `tests/perf_budget.rs`: perf-test pattern.
