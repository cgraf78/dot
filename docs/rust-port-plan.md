# Rust Port Historical Plan

This document records the migration approach used to replace the private Bash
engine with the native Rust `dot` CLI. It is not a list of remaining work. The
normative compatibility record is [`rust-port-spec.md`](rust-port-spec.md), and
measured results are in
[`rust-port-performance.md`](rust-port-performance.md).

## Goals

- Preserve commands, flags, help text, exit codes, output streams, configuration
  grammar, state formats, and repository behavior.
- Preserve the versioned public shell APIs used by hooks and executable test
  suites without retaining a second CLI engine.
- Make `dot update` substantially faster in realistic local-repository
  workloads.
- Ship prebuilt native binaries through the same reusable release conventions
  as the other `cgraf78` Rust repositories.
- Keep the implementation small enough that each behavior has one production
  owner.

## Boundaries

The Rust crate owns command dispatch, configuration, repository synchronization,
link publication, initialization and rollback, providers, hooks, doctor, and the
test coordinator. There is no production Bash fallback.

Shell remains appropriate only where shell is itself the interface:

- the release/bootstrap adapter;
- the public hook compatibility runtime sourced by user hook scripts;
- public API probes and executable acceptance suites;
- CI and release orchestration.

These files cannot dispatch or source the removed private engine.

## Migration sequence

The work was developed as a linear, unmerged PR stack:

1. Scaffold the Rust crate, command parser, tests, and reusable CI.
2. Port configuration, paths, logging, temporary state, and public contracts.
3. Port update, repository, overlay, link, profile, and merge phases.
4. Port Shdeps coordination, workers, trust validation, and hooks.
5. Port init transactions, doctor, test, and remaining commands.
6. Cut the launcher and release artifacts over to the native binary.
7. Delete the private Bash engine and replace differential tests with direct
   native regression and end-to-end tests.
8. Simplify unused migration seams and prove release-mode performance.

Every stack entry remains unmerged until its required checks are green. The
final cumulative entry is the authoritative review surface.

## Compatibility method

During migration, the historical Bash implementation supplied differential
fixtures. After cutover, tests do not source deleted engine files. Contracts are
pinned through literal unit cases, native integration fixtures, end-to-end CLI
runs, versioned TSV inventories, transaction failure tests, concurrency stress
tests, and retained shell-boundary acceptance suites.

Timing fields and spinner frames may be normalized in output comparisons. Exit
status, diagnostic wording, declaration-order replay, persistent state, and the
final filesystem tree remain significant.

## Performance method

Performance evidence has two layers:

- `hyperfine` compares explicit pre-cutover and native release executables for
  startup-oriented commands;
- `tests/perf_update.rs` measures complete native update fixtures and enforces a
  regression budget, while the archived pre-cutover harness supplies the same
  fixture's Bash baseline.

Measurements use local Git remotes so network variance is not mistaken for
engine cost. See [`rust-port-performance.md`](rust-port-performance.md) for
commands, sample counts, p95 values, and limitations.

## Completion gates

The port is complete only when all of the following hold at the same commit:

- all CLI and engine paths execute without the private Bash tree;
- native unit, integration, end-to-end, failure, and concurrency tests pass;
- retained shell-boundary suites pass with the private engine absent;
- formatting, strict Clippy, Rustdoc, provenance, installer, package, and
  release checks pass;
- the release artifact runs `help`, `version`, init, and update smoke tests;
- performance results show a substantial improvement rather than only meeting
  an absolute ceiling;
- fresh-eyes reviews find no unresolved correctness, security, portability,
  performance-methodology, or unnecessary-complexity issue;
- every PR in the linear stack is green, with no PR merged automatically.
