# SDD ledger — plan: docs/superpowers/plans/2026-09-06-rust-port-completion.md

## Preflight dependency and consistency scan

| Tasks | Shared file or interface | Finding |
|---|---|---|
| 1 | Existing parity harnesses consumed by all later tasks | Clean: stabilization precedes behavioral work. |
| 2, 3 | `Runtime` consumed by update entry behavior | Clean: context is established before native edge handling. |
| 2, 5 | `Runtime` and child-process environment | Clean: hook worker consumes immutable context. |
| 2, 6 | `Runtime` and Shdeps process environment | Clean: provider consumes immutable context. |
| 2, 7 | `app`, `cli`, and native update entry | Clean: unconditional update follows runtime introduction. |
| 2, 8 | `cli` and native application service | Clean: init consumes the established context. |
| 2, 9 | `Runtime` and `Streams` | Clean: doctor production coordinator uses the shared boundary. |
| 2, 10 | `Runtime`, `Streams`, and `cli` | Clean: test supervisor uses the shared boundary. |
| 2, 12 | `main`, startup, and installed entry | Clean: entry cutover follows stable runtime semantics. |
| 2, 13 | `lib.rs` public surface | Clean: privacy reduction follows all command wiring. |
| 3, 4 | `update_engine.rs` and update state | Clean: edge handling lands before profile state expansion. |
| 3, 7 | `update_run.rs` fallback path | Clean: edge fallbacks are removed before adapter deletion. |
| 4, 5 | `UpdateState` across pre-sync and merge phases | Clean: hooks consume final typed profile/overlay state. |
| 4, 6 | Update phase state and provider finalization | Clean: provider runs after profile state is explicit. |
| 4, 7 | Profile fallback removal | Clean: unconditional update waits for profile parity. |
| 5, 6 | Update orchestration and external-process boundary | Clean: independently reviewable coordinators. |
| 5, 7 | Hook and merge fallbacks | Clean: adapter deletion waits for both. |
| 5, 9 | Trusted extension worker | Clean: doctor reuses the narrow hook interpreter boundary. |
| 5, 10 | Trusted extension discovery | Clean: test discovery may reuse trust policy without sharing scheduling. |
| 6, 7 | Provider fallback and checkpoint behavior | Clean: unconditional update waits for provider parity. |
| 6, 8 | `DOT_INIT_SKIP_PROVIDER` behavior | Clean: init integration follows provider ownership. |
| 7, 8 | Native update application service | Clean: strict dependency is explicit. |
| 7, 12 | No-fallback assertion and shell deletion | Clean: shell deletion occurs only after native-only update. |
| 8, 12 | Init installed-entry behavior | Clean: installed cutover follows native init convergence. |
| 9, 10 | `run_engine_arm` deletion | Clean: adapter is removed only after both commands are native. |
| 9, 13 | Doctor record/renderer simplification | Clean: task 9 deletes exact duplicates; task 13 performs broader evidence-led cleanup. |
| 10, 12 | Retained public test reporter boundary | Clean: task 10 decides the external contract before engine deletion. |
| 10, 13 | Test-only public exports | Clean: privacy cleanup follows supervisor completion. |
| 11, 12 | Release archive and installer contract | Clean: release layout is defined before installation cutover. |
| 11, 14 | Release binaries used for benchmarks | Clean: measurement follows packaging. |
| 12, 13 | Bash deletion and transitional Rust cleanup | Clean: production deletion precedes final simplification. |
| 12, 14 | Archived shell oracle for final measurement | Conflict: task 12 deletes the engine while task 14 consumes an archived oracle. |
| 13, 14 | Final simplified code is benchmark subject | Clean: performance proof measures the final implementation. |
| 14, 15 | Raw benchmark evidence and final report | Clean: final monitoring consumes verified results. |
| 1 | Tests target the files named by the fix | Clean; exact source files are conditional on reproduced evidence. |
| 2 | Context tests match the proposed API | Clean. |
| 3 | Native edge tests match direct update changes | Clean. |
| 4 | State-machine tests match profile implementation | Clean. |
| 5 | Hook tests match worker and coordinators | Clean. |
| 6 | Provider matrix matches coordinator scope | Clean. |
| 7 | No-fallback test matches adapter deletion | Clean. |
| 8 | Init parity tests match convergence and lock work | Clean. |
| 9 | Doctor matrix matches production coordinator | Clean. |
| 10 | Lifecycle tests match supervisor responsibilities | Clean. |
| 11 | Contract tests match release assets | Clean. |
| 12 | Installation tests match entry cutover and rollback | Clean. |
| 13 | Inventory and compiler checks match simplification | Clean. |
| 14 | Harness validity tests match measurement design | Clean. |
| 15 | Verification steps match the no-merge delivery boundary | Clean. |

Ruling: Preserve a frozen test-only shell oracle before Task 12 deletes the production engine, and use only that immutable artifact in Task 14 — this keeps production shell-free while permitting an honest final A/B comparison; if wrong, the final performance comparison may need reconstruction from the last pre-deletion commit.

## Task progress

- Task 1 — COMPLETE. Code spec PASS and quality APPROVED after review;
  restacked #152/#153/#154/#159/#155/#156/#157 with lease-protected
  pushes, verified remote heads/bases, and monitored all checks to green
  (29/29 per ordinary head; 58/58 on shared cumulative #153/#154). All PRs
  remain open with auto-merge disabled. Final stack tip: `f2c7499`.

Ruling: Task 2 includes a dedicated transitive execution-context subtask
before acceptance. Independent review proved that a `Runtime` boundary limited
to CLI and `update_engine::gather` is false: native update still reaches
ambient PATH, TMPDIR, WSL detection, temporary allocation, and inherited child
environments through shared helpers. Use one explicit context and command
construction API across that reachable graph; do not hide the leak with locks
or defer it behind a public immutable-runtime claim. If wrong, this may widen
Task 2 beyond the smallest useful slice, but accepting a dishonest boundary
would create concurrency bugs and force later rework.

Ruling: Abandon the transitive `_with` wrapper implementation after its
16-file, +706/-151-line stop audit. Public embedded Runtime calls always
re-exec only an explicitly attached, validated executable with an
environment-cleared snapshot and cwd; they never compare or consult host
environment/cwd state. The ordinary binary entry captures env/cwd once and
uses direct dispatch. This keeps ambient native helpers correct inside the
isolated child and avoids plumbing execution parameters through the entire
repository graph. Treat the shell launcher's `bash-v1` state marker as
bootstrap metadata, not converged CLI state, in fallback parity. If wrong, an
embedding-specific child-spawn cost or executable-capability contract may need
reconsideration; ordinary CLI performance remains unaffected.

- Task 2 — LOCAL IMPLEMENTATION COMPLETE, pending controller review. The
  final process-isolation correction has `main` capture env/cwd once and enter
  direct dispatch, while public embedded Runtime calls require an explicit
  executable capability and always re-exec with an env-cleared map and cwd.
  Deterministic native overlap, force-fallback
  semantic parity, focused update/fleet tests, full Rust tests, ignored CI
  performance coverage, shell oracle, formatter, Clippy, rustdoc, and diff
  checks all have explicit local exit-0 evidence. No remote operation was
  performed.
