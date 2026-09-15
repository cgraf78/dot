# Rust Port Completion Design

## Objective

Complete the migration of `dot` from a Bash engine to a native Rust CLI while
preserving the observable behavior of the current shell implementation,
substantially improving performance, simplifying the Rust codebase, and keeping
the work as a linear stack of unmerged, green pull requests.

The final `dot` executable must not source or invoke the old Bash engine. Shell
remains permitted only outside the core engine for bootstrap, installation,
release packaging, CI, compatibility tests, and user-authored hooks.

## Delivery Boundary

- Do not merge or land any pull request.
- Repair and restack the existing port branches into one linear Git history.
- Keep incremental changes reviewable and monitor every affected pull request
  until its current head is green.
- End with a final cumulative cutover pull request that contains every intended
  lane, including release support, without merging it.

## Design Principles

1. Preserve behavior, not the Bash implementation structure.
2. Keep one native execution path for each command.
3. Keep policy pure where practical and isolate environment, filesystem, Git,
   process, clock, terminal, and platform access at explicit boundaries.
4. Prefer direct control flow and small typed records over shell-shaped helper
   layers or delimiter-encoded internal state.
5. Make modules and functions private unless they form an intentional library or
   compatibility interface.
6. Add abstractions only when they remove real duplication or enforce a safety
   boundary.
7. Delete migration scaffolding as soon as its replacement is proven.

## Final Architecture

`src/main.rs` remains a minimal process adapter. An internal application layer
parses the command line, constructs one immutable runtime context, dispatches a
command, streams output, and converts the result to a process exit code.

Native Rust owns all user-facing commands:

- `help` and `version`
- `update` and its `pull` alias
- `init`
- `fetch`, `push`, `status`, and `diff`
- `cron`
- `doctor`
- `test`

The application layer calls focused domain modules for configuration, repository
operations, overlay convergence, transactions, profiles, providers, hooks,
diagnostics, and test scheduling. Durable formats and public hook protocols keep
their existing encodings, but values become typed records after parsing and stay
typed until rendering or persistence.

User-provided hooks may execute with their declared interpreters. Rust owns hook
discovery, validation, ordering, concurrency limits, result collection, and
error handling. Hook execution does not permit the core engine to fall back to
the old Bash implementation.

## Remaining Native Work

The native update driver must absorb every current fallback lane:

- cron and cron-dirty handling;
- forced pull behavior;
- profile selection and two-phase convergence;
- Shdeps provider availability, ensure, update, checkpoint, and UI behavior;
- merge hooks and pre-sync hooks;
- configured merges;
- missing or invalid home handling;
- frozen or blocked filesystem replacement handling.

`doctor` and `test` must use their existing Rust kernels through native
orchestration. `init` must call the completed native update path for convergence.
After these paths are proven, remove `DOT_UPDATE_NATIVE`, `Fallback`, embedded
engine scripts, `run_engine_arm`, `CONVERGE_PENDING`, and all silent Bash
fallbacks.

## Simplification

The current port deliberately mirrors many Bash internals. That is useful while
establishing parity but is not the final structure.

After each native command becomes authoritative:

- remove the corresponding adapter and compatibility-only call path;
- delete helpers with no remaining production caller;
- combine one-use forwarding layers when they do not enforce an invariant;
- centralize duplicated parsing, process execution, filesystem safety, and
  transaction logic at their existing shared owners;
- reduce the public surface in `src/lib.rs` to intentional entry points;
- replace migration-history comments with current ownership and invariant
  documentation;
- split large modules only when the resulting parts have independent,
  understandable responsibilities;
- avoid framework-like registries, plugin systems, or generalized executors that
  are not required by current behavior.

There is no arbitrary line-count target. Completion requires an inventory showing
that each remaining production module has a current purpose, each public item has
an intended consumer, and no compatibility-only duplication remains. Report
before-and-after production lines, test lines, module count, public symbols,
binary size, and subprocess count for representative commands.

## Behavioral Parity Gate

The shell implementation remains a test oracle until every native path is
covered. Differential tests compare, as applicable:

- exit status, stdout, and stderr;
- final home-directory trees and file contents;
- symlink targets, modes, and ownership-sensitive decisions;
- overlay manifests and lifecycle records;
- Git worktree, index, branch, upstream, and skip-worktree state;
- transaction journals, recovery, rollback, and interrupted-run behavior;
- repeated clean convergence and idempotence;
- deterministic replay order for already-parallel operations.

Coverage must include happy paths, invalid input, dirty repositories, missing
tools, partial filesystem failures, permissions and ownership refusals, lock
contention, collisions, multiple overlays, hooks, profiles, providers, and
platform-specific behavior.

Every end-to-end native test must make old-engine invocation impossible. A test
that could silently pass through a Bash fallback is not native parity evidence.
The old shell engine may be deleted only after the complete native matrix passes.

## Performance Gate

Benchmark the old shell release and the new Rust release against independent but
identical isolated fixtures. Record host hardware, operating system, filesystem,
tool versions, build mode, fixture topology, warm-up policy, and sample count.

Measure at least:

- `help` and `version` startup;
- clean base-only update;
- clean and dirty multi-overlay update;
- colliding and disjoint overlay layouts;
- profiles, Shdeps provider, pre-sync hooks, merge hooks, and merges;
- representative failure paths.

Use enough independent samples to report meaningful median and p95 values. Dirty
measurements must reintroduce dirt before every sample. Both engines must produce
equivalent final state before their timings are compared. Tests must independently
prove that the native engine, not an adapter, was measured.

The cutover requires a substantial improvement in expensive update workloads,
not merely passing a loose absolute ceiling. Performance changes may not alter
ordering, error handling, safety checks, or output contracts.

## Cutover and Shell Removal

Once native parity, performance, and simplification gates pass:

1. Make the compiled Rust binary the installed and repository-supported `dot`
   entry point.
2. Remove the production Bash engine and all embedded/sourced engine adapters.
3. Retain only narrowly scoped shell assets for installation, packaging, CI,
   compatibility fixtures, and user hook examples where shell is the appropriate
   boundary.
4. Update documentation, examples, provenance, installation flows, and generated
   artifacts to describe the native architecture.
5. Package prebuilt artifacts for the same supported host families as the other
   cgraf78 Rust repositories so client machines do not require a Rust toolchain.
6. Run release dry-runs and package smoke tests without publishing a release from
   an unmerged branch.

## Release and CI Consistency

Use the same shared `cgraf78/actions` Rust CI and release family as the other
cgraf78 Rust repositories. The repository must include the standard pinned
release scripts, manifest, configuration, smoke hook, checksums, platform naming,
version derivation, and reusable workflow integration.

CI must retain shell-oracle coverage while the port is in progress and then keep
only the shell tests that validate supported non-engine boundaries. Required
checks cover formatting, Clippy, Rustdoc, locked tests, advisory audit, shell
linting, package smoke tests, Android/Termux contracts, and the supported Linux,
macOS, and WSL matrix.

## Pull Request Strategy

Continue from the current linear tip. Each remaining engine lane, simplification
unit, release repair, and final cutover is a focused PR based on the previous
branch. Rewrite published branches only with explicit expected-old-SHA
`--force-with-lease`, then verify both remote branch and PR head.

The cumulative PRs may remain as review endpoints, but their descriptions and
heads must accurately represent their contents. In particular, the release PR
must contain the advertised release files rather than aliasing a non-release
commit.

After each slice:

1. run focused tests and the complete locally reproducible gates;
2. perform an independent correctness and maintainability review;
3. fix material findings and repeat affected checks;
4. push and verify the exact remote head;
5. monitor its workflow rollup, distinguishing queued, cancelled, infrastructure,
   and actual test failures;
6. do not begin the next dependent slice until its base is green.

## Completion Criteria

The work is complete when:

- every user-facing command executes only through native Rust;
- the production Bash engine and all fallback paths are gone;
- differential parity passes across commands, state, failures, and platforms;
- the corrected benchmarks prove substantial release-mode improvement;
- the final Rust implementation has undergone a measured simplification pass;
- installation, release, CI, and package structure match the other cgraf78 Rust
  repositories;
- every affected PR is open, accurately described, linearly based, reviewed, and
  green at its exact remote head;
- no PR has been merged or landed.
