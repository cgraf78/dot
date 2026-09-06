# Task 2 report — Centralize the native runtime context

## Scope

Introduced the one immutable native invocation boundary required by Task 2:
`app::Runtime`, `app::Streams`, and `app::run`. The process adapter snapshots
the environment and working directory once; command routing consumes that
snapshot and passes command-specific values only to child processes. The shell
engine remains the behavior owner for temporary adapter paths.

## RED evidence

Before implementation, the two required tests failed because the public
application boundary did not exist:

- `cargo test --locked --test startup runtime_ -- --nocapture` exited `101`:
  the test could not resolve `dot::app`.
- `cargo test --locked --test cli
  app_runs_independent_contexts_without_mutating_process_environment` exited
  `101` for the same missing application module.

On resumption, the final mechanical Clippy cleanup was deliberately compiled
before broader verification. It was RED once with exit `101` for an unused
`root` local at `src/update_run.rs:186`; that binding was stale after grouping
runtime, state, and child environment into `UpdateContext`.

`cargo fmt --all -- --check` was also RED once for two line wraps in `cli.rs`.
Running the repository formatter produced no semantic edits.

## GREEN implementation

- Added `src/app.rs` with the intentionally public test/embedding interface:
  immutable `Runtime`, borrowed `Streams`, and `run`.
- `main` snapshots `vars_os` and `current_dir` once, constructs `Runtime`, and
  invokes `app::run`; it no longer mutates `DOT_SOURCE_ROOT`.
- `cli` keeps `run` as a temporary ambient compatibility adapter, while the
  production path enters `run_with_runtime` and all routed commands consume the
  supplied runtime.
- Replaced Task 2 command-path `set_var`/`remove_var` calls with an explicit
  child environment. Update flags and update-lock tokens reach shell children
  through `Command::env_clear().envs(...)`, without leaking into the parent.
- Routed startup, init, repository commands, and temporary engine arms through
  the runtime. `startup::check_ambient` remains a documented compatibility
  adapter for callers not yet migrated.
- Removed the now-dead `update_run::source_root` ambient helper in self-review.
  The runtime is the sole production owner of that resolution.
- Initially retained the pre-existing `update_engine.rs` environment mutation
  as Task 3-owned work. Review round 1 correctly found that the opt-in native
  lane is already reachable through `app::run`, so that boundary could not
  remain ambient. The correction below brings that capture under Task 2's
  immutable invocation contract without changing unsupported fallback routing.

## Tests and verification

All statuses below are explicitly captured.

- `cargo test --locked --test startup` — `13 passed`, exit `0`.
- `cargo test --locked --test cli` — `51 passed`, exit `0`.
- `cargo test --locked --lib update_run::tests` — `3 passed`, exit `0`.
- `cargo test --locked --test update_parpull` — `9 passed`, exit `0`
  (fresh focused run: `58.85s`; full-suite repeat: `59.63s`).
- `cargo test --locked` — exit `0`, including all unit, integration, and
  doc-tests.
- `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps` — exit `0`.
- `checkrun format` — exit `0`.
- `checkrun lint` — exit `0`.
- `bash tests/run` — `28 passed (28 total)`, exit `0`.
- Final `cargo fmt --all -- --check` — exit `0`.
- Final `cargo clippy --locked --all-targets --all-features -- -D warnings`
  — exit `0`.
- Final `git diff --check` — exit `0`.

## Review

Named self-review covered runtime immutability, public API size, child-process
environment propagation, shell adapter behavior, and command-path ambient
mutation. `Runtime` exposes only construction and the four path accessors;
environment/source-root access stays crate-private. `Streams` exposes only its
constructor. Integration tests exercise independent contexts and prove update
flags do not alter the caller's process environment.

No independent reviewer was dispatched because the controller explicitly
prohibited subagents for this task.

## Commit

- `f268110 Centralize the native runtime context`

The commit hook's required fast verification passed. Its optional spell-check
phase was unavailable only because intentional negative-test spellings
(`updat`, `fo\\xFFb`) are not dictionary words; no source or behavior concern
was reported. No branch was pushed, restacked, merged, or used to alter a pull
request.

## Historical concerns

- The original deferral of native update environment behavior to Task 3 was
  superseded by both review corrections below; it is retained here only to
  preserve the review chronology, not as a current scope claim.
- This task verified Linux-local behavior and the full shell oracle. macOS and
  other CI matrix execution remains a controller/CI follow-up and is not
  represented as local proof.

## Review round 1 — native update context isolation

The review found a real boundary violation: `update_run` selected
`DOT_UPDATE_NATIVE=1` from its invocation map, but `update_engine::gather`
then read and mutated the test process environment. Its later overlay,
reload-hint, topology, XDG, cwd, TMPDIR, and defensive startup paths could
also fall back to ambient values.

### RED evidence

Added `native_update_flag_capture_does_not_mutate_parent_environment` before
the implementation change. It builds two conflicting explicit Runtime
fixtures with `DOT_UPDATE_NATIVE=1`, HOME/XDG/topology/state roots unique to
each, and uses `update --quiet --force` so the existing native capture runs
but the native engine safely declines to the fixture-scoped shell fallback.

`cargo test --locked --test cli
native_update_flag_capture_does_not_mutate_parent_environment -- --exact
--nocapture` exited `101`. The assertion showed the process parent changed
`DOT_QUIET` and `DOT_FORCE` from absent to `"1"`.

### GREEN implementation

- `update_run` now passes its derived child environment and immutable Runtime
  cwd into `update_engine::gather`.
- `gather` reads every native environment input from that map and no longer
  calls `set_var`/`remove_var`. It constructs base topology from supplied
  values, uses the supplied cwd/TMPDIR/locale/PREFIX/freeze/reload values, and
  sends the supplied map to the `id -u` fallback child.
- `EngineInputs` owns the few later native inputs needed for overlay matching,
  reload hints, and defensive config reloads. Both reload points reuse one
  `startup_inputs` constructor and call `startup::preflight`, not
  `check_ambient`.
- The normal dispatcher remains the sole place that derives update flag
  exports; shell fallback routing, including `--force`, remains unchanged.
- Replaced the prior sequential help-only Runtime test with a genuinely
  concurrent test. It starts both explicit `DOT_UPDATE_NATIVE=1` updates
  before joining either, asserts both outputs and independent state/lock
  roots, and verifies the parent environment snapshot is unchanged. It uses
  no global lock or process-environment setup.

### GREEN verification

All statuses below were captured after the correction; the final full suite
started after the concurrency test was fixed to start both threads before
joining either.

- focused flag-leak regression — exit `0`.
- focused concurrent native contexts regression — exit `0`.
- `update_native_matches_shell_byte_for_byte` — exit `0`.
- full `tests/cli.rs` — `52 passed`, exit `0`.
- `cargo test --locked --test update_parpull` — `9 passed`, exit `0`
  (62.08s focused capture).
- final `cargo test --locked` — exit `0`; includes corrected concurrent test,
  update-parallel `9 passed`, update-run `4 passed`, startup `13 passed`, and
  doc-tests.
- `cargo fmt --all -- --check` — exit `0`.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` — exit
  `0`.
- `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps` — exit `0`.
- `checkrun format` and `checkrun lint` — both exit `0`.
- `bash tests/run` — `28 passed (28 total)`, exit `0`.
- final `git diff --check` — exit `0`.

### Review

Named self-review covered native/adapter boundary ownership, all former
`update_engine` ambient reads, native fallback preservation, the two reload
sites, concurrent fixture behavior, parent-state isolation, public API scope,
and final diff hygiene. No independent reviewer was dispatched because the
controller expressly prohibited subagents.

### Commit

- `74c2759 Isolate native \`update\` runtime context`

No remote, pull request, restack, merge, or auto-merge operation is
authorized or performed.

## Review round 2 — transitive execution isolation

The second independent review correctly rejected the prior report's claim that
the `Runtime` boundary isolated reachable native update work. Although the
first correction removed direct mutation in `update_engine::gather`, the
reachable graph still used process-ambient `PATH`, `TMPDIR`, WSL markers, and
inherited child command environments through shared helpers such as move-cache
and fleet pull. That was a real concurrency boundary defect, not Task 3-only
cleanup.

### Abandoned approach

I first audited and started an explicit execution-context/command-builder
threading approach. Before it was committed, the audit showed 16 modified
files and `+706/-151` lines of `_with` plumbing. It was stopped and preserved
recoverably in a local stash, then the tracked worktree was restored exactly to
`74c2759788e02215f7aa1e72cbca0e6209330678`. This report does not present that
abandoned implementation as verification or delivered code.

### RED evidence

Before the final implementation, the deterministic concurrent native-context
test entered `app::run` directly. Both explicit Runtime maps used distinct
HOME/XDG roots, `PATH` shim directories, `TMPDIR` roots, and WSL markers. The
shim held both overlay fetch workers after fleet scratch allocation. The test
failed with exit `101`: embedded Runtime calls ran in the parent process and
missed the Git barrier.

The explicit executable-resolution regression also failed with exit `101`:
an embedded Runtime whose `DOT_RUNTIME_EXECUTABLE` was
`/nonexistent/dot-runtime-child` returned help success rather than a clear
child-launch error.

### GREEN implementation

- `app::run` now compares its immutable Runtime snapshot with the actual
  process environment and cwd. The normal `main` snapshot matches and calls
  `cli::run_with_runtime` directly, with no spawn or performance cost.
- A differing/embedded Runtime re-execs the Rust `dot` executable with
  `Command::env_clear().envs(runtime.env()).current_dir(runtime.cwd())`, then
  forwards the exact captured stdout, stderr, and exit status. The child
  snapshots that same process and therefore takes the direct path, preventing
  recursion.
- `DOT_RUNTIME_EXECUTABLE` is the narrow embedding/test executable-resolution
  input; production defaults to `current_exe`. Resolution and launch failures
  return a clear diagnostic and exit `1`.
- The concurrency regression uses the actual compiled `dot` binary, a bounded
  barrier at a real native overlay-fetch seam, and per-context shim traces. It
  proves overlapping workers see their own `PATH`, `TMPDIR`, and WSL markers,
  use their own fleet scratch directories, produce independent outputs/state,
  and leave the parent environment unchanged.
- The `--force` fallback regression now compares exit status, stdout, stderr,
  user tree, and state tree with the shell oracle. Its only classification is
  the shell launcher's exact `dot/bash-v1` bootstrap metadata (under either
  HOME state or an explicit state root); no marker is recreated and no broad
  state normalization is used.

### GREEN verification

All statuses below were captured after the process-isolation replacement.

- isolated concurrent Runtime regression — `1 passed`, exit `0` (4.49s).
- force-fallback semantic parity regression — `1 passed`, exit `0` (5.17s).
- executable-resolution regression — `1 passed`, exit `0`.
- `cargo test --locked --test cli` — `53 passed`, exit `0` (8.23s).
- `cargo test --locked --test update_parpull` — `9 passed`, exit `0`
  (58.91s).
- `cargo test --locked --test repos_pull_fleet` — `8 passed`, exit `0`
  (37.33s).
- `cargo test --locked` — exit `0`, including unit, integration, and
  doc-tests.
- `DOT_PERF_BUDGET_MULTIPLIER=1 cargo test --locked -- --ignored` — exit `0`;
  `clean_and_dirty_update_within_budget` passed in 61.68s.
- `bash tests/run` — `28 passed (28 total)`, exit `0`.
- `cargo fmt --check` — exit `0`.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` — exit
  `0`.
- `RUSTDOCFLAGS='-D warnings' cargo doc --locked --no-deps` — exit `0`.
- `git diff --check` — exit `0`.

### Final self-review

Named self-review covered the direct-versus-embedded recursion guard,
complete-environment equality, executable resolution and error forwarding,
byte-preserving stream forwarding, force-fallback bootstrap classification,
the real concurrent native seam, parent non-mutation, public API size, and
the absence of remote changes. No independent reviewer was dispatched because
the controller expressly prohibited subagents. The direct process path remains
the existing runtime behavior; only callers supplying a differing immutable
Runtime incur a child process.

### Delivery

The prior concern saying native update process-environment mutation was safely
deferred to Task 3 is superseded by this review round: reachable native update
now executes in the Runtime's isolated child process. No remote, pull request,
restack, merge, close, or auto-merge operation was performed.

### Commit

- `0f0cfda Isolate embedded native runtime execution`
