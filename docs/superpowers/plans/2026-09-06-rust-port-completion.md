# Rust Port Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the complete Bash `dot` engine with a simpler, substantially faster native Rust CLI while preserving feature and byte-level behavioral parity, then leave every pull request open, linear, and green.

**Architecture:** Build one immutable runtime context at process entry and route every command through native Rust domain modules. Retain shell only at explicit non-engine boundaries such as bootstrap, packaging, CI, compatibility tests, and user-authored hooks; delete adapters and migration scaffolding as soon as their native replacements are proven.

**Tech Stack:** Rust 2024 with MSRV 1.85 and the standard library, Bash compatibility oracles, Git, `cargo`, `checkrun`, GitHub Actions through `cgraf78/actions`, and the repository's existing fixture framework.

**Spec:** `docs/superpowers/specs/2026-09-06-rust-port-completion-design.md`

## Global Constraints

- Do not merge or land any pull request.
- Keep one linear, merge-free Git history based on the exact current head of PR #157.
- Push rewritten published branches only with explicit expected-old-SHA `--force-with-lease` and verify the remote branch and PR head afterward.
- Preserve all observable shell behavior unless the approved spec explicitly permits a difference.
- Add a failing differential or regression test before every behavior change.
- Use Rust 1.85-compatible Rust 2024 code and add no dependency without a demonstrated standard-library gap.
- Keep modules and functions private by default and prefer direct code over speculative abstractions.
- Do not permit a native parity test to pass by silently invoking the old Bash engine.
- Run focused tests, `cargo fmt --check`, strict Clippy, Rustdoc, `cargo test --locked`, the applicable shell oracle suites, package smoke tests, and CI in proportion to each slice.
- Independently review every slice for correctness and simplification before pushing it.

---

### Task 1: Stabilize the Existing Linear Stack

**Files:**

- Modify: `tests/update_parpull.rs`
- Modify: `tests/link_all.rs`
- Modify only if evidence requires it: `src/repos_link_all.rs`, `src/progress_ui.rs`
- Verify: PRs #151-#159 and their exact workflow runs

**Interfaces:**

- Consumes: elapsed-output normalization helpers already used by differential tests
- Produces: deterministic cross-platform parity assertions with no suppression of semantic output differences
- [ ] **Step 1: Reproduce each current CI failure from its exact PR head**

Run the failing tests on the corresponding branches and platforms where locally available:

```bash
cargo test --locked --test update_parpull clean_three_overlay_update_matches_shell_and_converges -- --nocapture
cargo test --locked --test update_parpull pushed_change_converges_on_three_overlays -- --nocapture
cargo test --locked --test link_all counted_ui_matches_shell -- --nocapture
```

Capture the first differing semantic field after existing elapsed-time and fixture-path normalization. Do not classify a failure as timing-related until the differing bytes prove it.

- [ ] **Step 2: Add a regression that fails repeatedly under the affected clock boundary**

Extract or extend the normalization test so inputs such as these compare equal while status text remains significant:

```rust
let left = b"[1/4] Overlays   ok       1 overlay current                          0s\n";
let right = b"[1/4] Overlays   ok       1 overlay current                          1s\n";
assert_eq!(normalize_elapsed(left), normalize_elapsed(right));
```

Run the focused test before the fix and verify failure.

- [ ] **Step 3: Apply the smallest root-cause fix**

Reuse the single canonical elapsed normalizer. Do not add retries around a deterministic mismatch and do not normalize counts, statuses, labels, ordering, or diagnostics.

- [ ] **Step 4: Verify the affected slices**

Run each failing test at least ten times, then its full test binary and `cargo test --locked`. Re-query current workflow heads and distinguish superseded cancellations from real failures.

- [ ] **Step 5: Commit, safely restack affected descendants, push, and monitor**

Use a commit titled `Stabilize cross-platform parity timing assertions`. Update each affected branch with leases, verify exact SHAs, and do not proceed until the repaired bases are green.

### Task 2: Introduce One Native Runtime Boundary

**Files:**

- Create: `src/app.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Modify: `src/cli.rs`
- Modify: `src/startup.rs`
- Test: `tests/cli.rs`
- Test: `tests/startup.rs`

**Interfaces:**

- Produces: `app::Runtime`, `app::Streams<'a>`, and `app::run(&Runtime, &[OsString], &mut Streams<'_>) -> i32`
- Consumes: existing config, XDG, platform, logger, and command implementations
- [ ] **Step 1: Write failing construction and concurrency-boundary tests**

Pin an immutable snapshot interface:

```rust
let runtime = Runtime::from_env(&env, &cwd).expect("runtime");
assert_eq!(runtime.home(), home.as_path());
assert_eq!(runtime.state_home(), state.as_path());
```

Add a CLI test that runs two independently constructed contexts without either mutating process environment.

- [ ] **Step 2: Verify the new tests fail because no runtime boundary exists**

Run `cargo test --locked --test startup runtime_ -- --nocapture` and retain the missing-type or mutation failure.

- [ ] **Step 3: Implement the minimal immutable context**

Use owned values at the process boundary and borrowed accessors internally:

```rust
pub(crate) struct Runtime {
    home: PathBuf,
    state_home: PathBuf,
    config_home: PathBuf,
    cwd: PathBuf,
    env: BTreeMap<OsString, OsString>,
}
```

Read ambient environment once in `main`; do not call `set_var` or `remove_var` from command implementations.

- [ ] **Step 4: Route existing commands through the context without behavior changes**

Keep existing adapters temporarily, but pass their required environment explicitly to child `Command` objects. Make implementation modules `pub(crate)` where external tests do not require them.

- [ ] **Step 5: Verify and commit**

Run CLI/startup tests, strict Clippy, Rustdoc, and the full locked suite. Commit as `Centralize the native runtime context`.

### Task 3: Remove Obsolete Update Fallbacks and Port Cron Handling

**Files:**

- Modify: `src/update_engine.rs`
- Modify: `src/update_run.rs`
- Modify: `src/repos_dirty.rs`
- Test: `tests/update_run.rs`
- Test: `tests/repos_dirty.rs`
- Test: `tests/cli.rs`

**Interfaces:**

- Consumes: `repos_dirty::is_worktree_dirty`, `repos_dirty::try_resolve_dirty`, parsed `UpdateFlags`
- Produces: native cron, force-with-provider-none, invalid-home, and fresh-run filesystem-replacement behavior
- [ ] **Step 1: Add failing native end-to-end cases**

Cover clean cron, mtime-only dirty cron, unresolved dirty cron with silent exit 0, `--force` with provider `none`, missing/relative HOME with absolute and absent XDG roots, and ambient `DOT_OVERLAY_LINKS_FROZEN=1` reset.

Each case must poison old-engine execution:

```rust
fixture.break_shell_engine();
let output = fixture.rust_dot(&["update", "--cron"]);
assert_eq!(output.status.code(), Some(0));
assert!(output.stdout.is_empty());
assert!(output.stderr.is_empty());
```

- [ ] **Step 2: Verify the cases fail through current fallback behavior**

Run the named native tests with `--nocapture` and confirm the poison marker is observed.

- [ ] **Step 3: Implement native behavior directly**

Remove `ForcePull` and `FsReplaceBlocked` as obsolete top-level fallbacks. Represent HOME/XDG failures as typed native errors. Run cron dirty resolution before visible UI and return silently when unresolved.

- [ ] **Step 4: Verify parity and commit**

Run focused Rust tests, `bash tests/update-test`, the CLI suite, and the full locked suite. Commit as `Port update entry edge cases to Rust`.

### Task 4: Port Profile and Two-Phase Overlay Convergence

**Files:**

- Modify: `src/update_engine.rs`
- Modify: `src/overlays.rs`
- Modify: `src/profiles.rs`
- Modify: `src/profile_lifecycle.rs`
- Test: `tests/update_run.rs`
- Test: `tests/overlays.rs`
- Test: `tests/profiles.rs`
- Test: `tests/profile_lifecycle.rs`

**Interfaces:**

- Consumes: `profiles::State`, `overlays::resolve`, lifecycle prepare/retire/commit
- Produces: `UpdateState` carrying selected, eligible, active, prior, retained, and lifecycle records across sync and finalize
- [ ] **Step 1: Add failing full-native profile fixtures**

Cover base-selected overlays, additions discovered after base pull, selector conflicts, profile downgrade, deactivation success/failure, and rollback. Compare final trees, manifests, lifecycle ledgers, streams, and Git state to the shell oracle.

- [ ] **Step 2: Verify failure with the Bash engine poisoned**

Run each new native test individually and confirm `Fallback::Profiles` is the reason it cannot complete.

- [ ] **Step 3: Implement one explicit two-phase state machine**

Add a focused state record rather than parallel vectors:

```rust
struct UpdateState {
    profiles: profiles::State,
    selected: BTreeSet<String>,
    eligible: BTreeSet<String>,
    active: BTreeSet<String>,
    prior: BTreeSet<String>,
    retained: BTreeSet<String>,
}
```

Execute shell-compatible ordering: load/select, first resolution, snapshot and prepare, first pull, resolve defaults, reconcile, additions-only pull, final discovery, then finalize.

- [ ] **Step 4: Delete the profile fallback and verify**

Run all profile/overlay/lifecycle tests, the shell profile-resolution suite, native update tests, and full locked tests. Commit as `Port profile convergence to the native update engine`.

### Task 5: Port Pre-Sync and Merge Hook Coordination

**Files:**

- Create: `src/hook_worker.rs`
- Modify: `src/pre_sync.rs`
- Modify: `src/merges.rs`
- Modify: `src/update_engine.rs`
- Modify: `src/extension_trust.rs`
- Test: `tests/pre_sync.rs`
- Test: `tests/merges.rs`
- Test: `tests/update_run.rs`
- Verify: `tests/extension-worker-context-test`, `tests/extensions-api-test`

**Interfaces:**

- Produces: `hook_worker::run(&Runtime, &HookSpec, &OverlayContext) -> HookResult`
- Consumes: trusted hook specs, one-use overlay contexts, job limits, deterministic result prefixes
- [ ] **Step 1: Add failing native hook integration tests**

Cover trusted success, unsafe file refusal, failure diagnostics, context consumption, sequential pre-sync ordering, merge parallel batches, `.serial.sh` barriers, quiet/verbose rendering, and deterministic replay.

- [ ] **Step 2: Verify tests fail at current `PreSyncHooks` and `MergeHooks` fallbacks**

Poison the old engine and run the new cases.

- [ ] **Step 3: Implement the narrow interpreter boundary**

Spawn only the user-authored hook through its validated interpreter. Rust owns discovery, validation, environment construction, ordering, process supervision, capture, and rendering. The worker must never source `lib/dot/update.sh`, `commands.sh`, or `main.sh`.

- [ ] **Step 4: Compose pre-sync and merges natively**

Use bounded workers for parallel merge batches, join before serial barriers, store indexed results, and replay in declaration order. Remove the corresponding update fallbacks.

- [ ] **Step 5: Stress, verify, and commit**

Run the hook tests repeatedly under `DOT_UPDATE_JOBS=1` and a multi-worker value, all extension API shell tests, full locked tests, and Clippy. Commit as `Run update hooks through the native coordinator`.

### Task 6: Port the Shdeps Provider Lifecycle

**Files:**

- Create: `src/shdeps_provider.rs`
- Modify: `src/shdeps.rs`
- Modify: `src/shdeps_env_abi.rs`
- Modify: `src/shdeps_ui.rs`
- Modify: `src/shdeps_ui_render.rs`
- Modify: `src/update_engine.rs`
- Test: create `tests/shdeps_provider.rs`
- Verify: `tests/shdeps-provider-test`

**Interfaces:**

- Produces: `shdeps_provider::ensure`, `shdeps_provider::update`, and typed checkpoint/re-exec outcomes
- Consumes: existing trust, ABI, environment, bounded-run, checkpoint, and UI kernels
- [ ] **Step 1: Port the shell provider matrix into failing Rust integration cases**

Include provider `none`, preselected binary, managed release, approved development checkout, missing/unavailable binary, installer failure, ABI mismatch, timeout, quiet/force propagation, checkpoint write/consume, and one-generation re-exec.

- [ ] **Step 2: Verify all provider-enabled native updates fail closed**

Break the Bash engine and confirm current `ShdepsProvider` behavior cannot pass.

- [ ] **Step 3: Implement the smallest coordinator around existing kernels**

Treat Shdeps as an external executable/API boundary. Do not source the old dot provider. Preserve validated binary selection, bounded invocation, event/UI rendering, checkpoint publication, and re-exec semantics.

- [ ] **Step 4: Remove the provider fallback and verify**

Run all `shdeps*` Rust tests, the complete shell provider oracle, native update cases, and full locked tests. Commit as `Port the Shdeps update provider to Rust`.

### Task 7: Make Native Update Unconditional

**Files:**

- Modify: `src/update_engine.rs`
- Modify: `src/update_run.rs`
- Modify: `src/cli.rs`
- Modify: `src/app.rs`
- Test: `tests/update_run.rs`
- Test: `tests/update_parpull.rs`
- Test: `tests/cli.rs`

**Interfaces:**

- Changes: `update_engine::run_update(&Runtime, &UpdateRequest, &mut Streams<'_>) -> i32`
- Removes: `Fallback`, `should_go_native`, `DOT_UPDATE_NATIVE`, embedded `ENGINE_SCRIPT`, `run_engine`, and optional gather/run results
- [ ] **Step 1: Add a repository-wide no-fallback assertion**

Create a test fixture with the old engine files absent and exercise every update envelope. Search production Rust for `DOT_UPDATE_NATIVE`, `ENGINE_SCRIPT`, and old engine paths; the test fails while any runtime dependency remains.

- [ ] **Step 2: Verify failure before deletion**

Run the no-fallback test and record the adapter dependency.

- [ ] **Step 3: Route update directly through the native driver**

Replace optional gathering with typed `Result`, stream output through `Streams`, and return native exit codes. Remove all fallback code and process-global environment mutation from this path.

- [ ] **Step 4: Verify full update parity and commit**

Run all update, repository, overlay, profile, provider, hook, merge, lock, transaction, CLI, shell-oracle, and ignored performance correctness tests. Commit as `Make the update engine fully native`.

### Task 8: Complete Native Init Convergence and Locking

**Files:**

- Modify: `src/cli.rs`
- Modify: `src/init_client_engine.rs`
- Modify: `src/init_client_command.rs`
- Modify: `src/update_lock.rs`
- Test: `tests/init_client_engine.rs`
- Test: `tests/init_client_command.rs`
- Test: `tests/cli.rs`
- Verify: `tests/init-test`, `tests/ownership-transfer-test`

**Interfaces:**

- Consumes: unconditional native update application function and existing update lock guard
- Removes: `CONVERGE_PENDING` and diagnostic-only converge plumbing
- [ ] **Step 1: Add failing native init-to-update parity cases**

Exercise fresh, adopt, resume, rollback, provider-skip, lock contention, and `init → update → repeat update`, with the Bash engine unavailable and complete state comparisons.

- [ ] **Step 2: Verify current pending convergence fails**

Run the new tests and assert the existing `CONVERGE_PENDING` path is reached.

- [ ] **Step 3: Bind initialization to native convergence**

Call the native application service without recursively invoking the CLI. Hold one operation lock across init and convergence without reacquisition; retain lock skipping only for status/help modes.

- [ ] **Step 4: Remove pending scaffolding, verify, and commit**

Run every init/transaction/rollback/lock test and relevant shell oracle. Commit as `Complete native init convergence`.

### Task 9: Wire Doctor Natively

**Files:**

- Create: `src/doctor.rs`
- Modify: `src/doctor_orchestrator.rs`
- Modify: `src/doctor_coordinator.rs`
- Modify: `src/doctor_records.rs`
- Modify: `src/cli.rs`
- Test: create `tests/doctor.rs`
- Verify: all `tests/doctor_*.rs`, `tests/doctor-test`, `tests/extension-worker-context-test`

**Interfaces:**

- Produces: `doctor::run(&Runtime, &mut Streams<'_>) -> i32`
- Consumes: existing doctor checks, paths, runtime, records, trusted extension worker
- [ ] **Step 1: Add failing no-engine doctor parity cases**

Cover healthy and failing source/base/lock/provider/overlay/lifecycle/merge states, unsafe and malformed extensions, profile context, linked worktrees, and ignored discovery failure.

- [ ] **Step 2: Verify the current CLI requires `run_engine_arm`**

Remove the engine from the fixture and observe failure.

- [ ] **Step 3: Add one production coordinator**

Resolve overlays in tolerated inspect mode, build typed check inputs, run trusted extension checks through the hook boundary, and render through one canonical recorder.

- [ ] **Step 4: Delete duplicate doctor concepts encountered in the path**

Keep one `Record`, `Kind`, counts model, and renderer when duplicates have identical ownership. Do not combine unrelated checks simply to reduce file count.

- [ ] **Step 5: Verify and commit**

Run the full doctor and extension matrices plus locked tests, Clippy, and Rustdoc. Commit as `Run dot doctor through the native coordinator`.

### Task 10: Implement the Native Test Supervisor

**Files:**

- Create: `src/test_command.rs`
- Create: `src/test_runner.rs`
- Modify: `src/test_suites.rs`
- Modify: `src/cleanup.rs`
- Modify: `src/cli.rs`
- Test: create `tests/test_command.rs`
- Test: create `tests/test_runner.rs`
- Verify: `tests/test-command-test`, `tests/test-lifecycle-test`, `tests/extensions-api-test`

**Interfaces:**

- Produces: `test_command::run(&Runtime, &[OsString], &mut Streams<'_>) -> i32`
- Consumes: suite classification/filtering/jobs/timeouts, cleanup registry, trusted extension discovery
- [ ] **Step 1: Add failing argument, discovery, and lifecycle tests**

Cover help/list/filter, invalid jobs, source-home authority, local/provider/extension suites, changed-after-discovery refusal, sequential and bounded parallel order, timeout, TERM-to-KILL escalation, abrupt cancellation, descendant cleanup, malformed results, stale-root pruning, closed stdin, and concurrent invocations.

- [ ] **Step 2: Verify the tests cannot pass without the shell supervisor**

Break `lib/dot/test.sh` and its subordinate files in the fixture.

- [ ] **Step 3: Implement native discovery and scheduling**

Use explicit suite records and a bounded worker loop. Revalidate immediately before spawn. Give each child its own process group, poll observable completion to a deadline, terminate then kill on timeout, and collect indexed results for deterministic presentation.

- [ ] **Step 4: Preserve language-neutral reporting boundaries**

Retain only the public reporter or timeout helper that external suites genuinely consume. Rust owns supervision and never sources the shell test engine.

- [ ] **Step 5: Stress, verify, and commit**

Run concurrency and cancellation tests repeatedly, the complete lifecycle shell oracle, full locked tests, Clippy, and Rustdoc. Commit as `Run dot test with the native supervisor`.

### Task 11: Add Standard Release and Package Infrastructure

**Files:**

- Create: `.github/workflows/release.yml`
- Create: `scripts/release.conf`
- Create: `scripts/release-lib.sh`
- Create: `scripts/release.sh`
- Create: `scripts/release-version.sh`
- Create: `scripts/release-tag.sh`
- Create: `scripts/package-release.sh`
- Create: `scripts/smoke-release.sh`
- Create: `scripts/release-smoke-hook.sh`
- Create: `scripts/.release-scripts.manifest`
- Modify: `.github/workflows/test.yml`
- Modify: `.github/shellcheck-files.txt`
- Test: create `tests/release-scripts-test`

**Interfaces:**

- Consumes: current vendored release family at the repository's pinned `cgraf78/actions` revision
- Produces: host release archives containing the native `dot` binary and retained public assets
- [ ] **Step 1: Add failing release-contract tests**

Assert required scripts, manifest hashes, configuration values, workflow inputs, archive layout, version/help execution, and absence of the production Bash engine from the package.

- [ ] **Step 2: Verify the current release branch fails because assets are absent**

Run `bash tests/release-scripts-test` before adding files.

- [ ] **Step 3: Vendor the canonical scripts and add dot-owned configuration**

Use `RELEASE_ENV_PREFIX=DOT`, `RELEASE_SLUG=dot`, `RELEASE_REPO=cgraf78/dot`, `RELEASE_ASSET_NAME=dot`, and `RELEASE_BINARY=dot`. Do not fork shared release logic into repo-owned variants.

- [ ] **Step 4: Wire package smoke across supported platforms**

Exercise host, musl where supported, Android/Termux, macOS, and the existing WSL contract. The smoke hook must run the extracted binary and prove it does not resolve `lib/dot`.

- [ ] **Step 5: Verify, repair PR #154 accurately, and commit**

Run the release dry-run, package/smoke scripts, shellcheck, provenance, workflow tests, and full local gates. Commit as `Add native dot release packaging`, then update #154 or create the correct incremental release PR so its title and content agree.

### Task 12: Cut Over Installation and Delete the Bash Engine

**Files:**

- Modify or replace: `bin/dot`
- Modify: `install.sh`
- Modify: `support/install-checkout.sh`
- Modify: `support/client-launcher.sh`
- Modify: `support/post-install.sh`
- Delete: production engine files under `lib/dot/` except explicitly retained public hook/test interfaces
- Modify: `.github/shellcheck-files.txt`
- Modify: `docs/source-provenance-v1.tsv`
- Test: `tests/install-test`
- Test: `tests/client-launcher-test`
- Test: `tests/startup.rs`
- Test: `tests/cli.rs`

**Interfaces:**

- Produces: installed prebuilt native `dot` with atomic upgrade and last-good retention
- Consumes: release archive metadata and retained public hook protocol assets
- [ ] **Step 1: Add failing installed-binary and rollback tests**

Cover fresh install, managed upgrade, development checkout, collision, symlink, race, offline retention, corrupt or wrong-version artifact rollback, post-install init, and execution with all old engine files absent.

- [ ] **Step 2: Verify the shell-era launcher assumptions fail**

Run install and launcher tests against a package containing only the compiled binary and retained public assets.

- [ ] **Step 3: Implement the simplest native installation path**

Select the platform artifact, verify checksum and repository attestation through the shared release contract, stage in the destination directory, run `dot version`, and atomically rename. Never delete the last working binary before validation.

- [ ] **Step 4: Remove the production Bash engine**

Delete the shell command dispatcher, update engine, repository engine, providers, doctor engine, test supervisor, and their source-only helpers after their native replacements pass. Keep only reviewed non-core shell boundaries.

- [ ] **Step 5: Prove no runtime dependency remains and commit**

Search production code, packages, and installed fixtures for engine paths and shell fallback markers. Run every command with `lib/dot/main.sh` absent. Commit as `Cut over dot to the native Rust engine`.

### Task 13: Simplify the Final Rust Implementation

**Files:**

- Modify: `src/lib.rs`
- Modify: oversized or fragmented `src/*.rs` modules identified by call-graph evidence
- Modify: affected `tests/*.rs`
- Modify: `AGENTS.md`
- Modify: `docs/rust-port-plan.md`
- Modify: `docs/rust-port-spec.md`

**Interfaces:**

- Produces: a small intentional public API and one owner per domain concept
- Removes: migration-only public exports, wrappers, adapters, delimiter state, duplicated records, and stale slice commentary
- [ ] **Step 1: Record the before inventory**

Measure production/test LOC, module count, public items, binary size, and representative subprocess counts. Save the reproducible commands and raw values in the final report.

- [ ] **Step 2: Make implementation modules private and run the compiler**

Reduce `lib.rs` to the intentional application interface. Move white-box integration tests behind a narrow test-support facade or into owning modules. Treat compiler failures as the caller inventory.

- [ ] **Step 3: Delete dead and transitional code**

Use `rg` call-site searches, compiler dead-code diagnostics, provenance ownership, and tests. Delete one-use pass-through layers unless they enforce a named invariant. Consolidate duplicate doctor records/renderers and slice-created init/repository fragments only where ownership becomes clearer.

- [ ] **Step 4: Review every large module for a simple responsibility boundary**

For each file over roughly 1,000 lines, either split it along existing domain responsibilities or record why it is cohesive. Do not introduce generic frameworks or empty trait indirection.

- [ ] **Step 5: Verify behavior after each deletion batch**

Use small commits and run the focused suite plus full locked tests after every batch. Finish with strict Clippy, Rustdoc, remaining shell-boundary tests, and an independent simplification review.

- [ ] **Step 6: Record the after inventory and commit**

Report absolute and percentage reductions without an arbitrary target. Commit cohesive deletion units with imperative titles.

### Task 14: Produce Rigorous Performance Evidence

**Files:**

- Modify: `tests/perf_update.rs`
- Create: `scripts/benchmark-port.sh`
- Create: `docs/rust-port-performance.md`
- Modify: `.github/workflows/test.yml`

**Interfaces:**

- Produces: paired shell-oracle/native raw results and a reproducible p50/p95 report
- Consumes: archived pre-cutover shell oracle built from the final parity baseline and the release-mode native binary
- [ ] **Step 1: Add failing harness-validity tests**

Assert every timed dirty sample is independently dirtied, both engines use equivalent isolated fixtures, final state agrees before timing is accepted, release binaries are used, native engagement is independently proven, and sample order alternates or is deterministically balanced.

- [ ] **Step 2: Verify the existing five-run mixed-dirty harness fails**

Run the validity tests against the current fixture.

- [ ] **Step 3: Implement paired measurement with raw output**

Use at least 30 measured samples after warm-up. Emit machine-readable rows containing engine, workload, iteration, elapsed nanoseconds, order, commit, OS, CPU, and filesystem. Compute median and nearest-rank p95 outside the timed region.

- [ ] **Step 4: Measure representative workloads**

Include startup, base-only clean update, multi-overlay clean and genuinely dirty updates, colliding and disjoint layouts, profiles, Shdeps, hooks/merges, and representative failures. Reset fixtures outside timed regions.

- [ ] **Step 5: Enforce and document substantial improvement**

Require at least 25% paired-median improvement for representative update workloads, plus stable absolute p95 ceilings derived from measured hosts. If the native path misses the threshold, profile and optimize the dominant native owner without relaxing parity.

- [ ] **Step 6: Verify and commit**

Run the benchmark on the reference host and CI performance hosts, validate raw data and report calculations independently, and commit as `Prove native dot performance gains`.

### Task 15: Final Review, Restack, and Green Monitoring

**Files:**

- Modify only for validated review findings: affected source, tests, docs, PR descriptions
- Verify: every open port PR and exact remote head

**Interfaces:**

- Produces: an open, linear, reviewed, all-green PR stack with no merges

- [ ] **Step 1: Run the complete local gate from a clean worktree**

Run formatting, strict Clippy, Rustdoc, locked tests, ignored performance tests, retained shell tests, release dry-run, package smoke, provenance, and workflow validation. Preserve every command's status.

- [ ] **Step 2: Dispatch independent reviews on complementary axes**

Require correctness/parity, security/transaction safety, concurrency/process lifecycle, portability/release, performance methodology, and simplification/API reviews. Fix every reproducible material finding and repeat review until clean.

- [ ] **Step 3: Verify requirements from the design and plan**

Check off every command, fallback, state format, platform, release asset, shell deletion boundary, simplification metric, and performance workload with current evidence.

- [ ] **Step 4: Restack and publish without merging**

Record each remote old SHA, push explicit branch destinations with leases, verify PR heads/bases/descriptions, and confirm one merge-free ancestry from `main` through the final tip.

- [ ] **Step 5: Monitor every PR to green**

Poll workflow rollups until all required and applicable checks complete successfully. Diagnose actual test failures at their owning slice, restack descendants after fixes, and repeat. Do not enable auto-merge and do not merge or close any PR.

- [ ] **Step 6: Report the final state**

Provide the ordered PR list, exact tip SHA, test totals, supported platforms, before/after code-size metrics, binary/package details, benchmark p50/p95 and improvements, retained shell boundaries, and explicit confirmation that no PR was landed.
