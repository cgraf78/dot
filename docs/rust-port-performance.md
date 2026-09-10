# Rust port performance

The required `Performance / Ubuntu` check compares the native release binary
with the final Bash engine on isolated clients backed by the same local Git
remotes. It is both a semantic gate and a timing gate: no timing sample is
accepted until the corresponding update has produced the expected state.

## Reproducible baseline

[`support/performance-baseline-v1.tsv`](../support/performance-baseline-v1.tsv)
is the single source of truth for the historical engine revision. It pins the
reachable commit immediately before native cutover. The benchmark driver makes
an independent detached clone at that revision so the shell can resolve its
own Git identity and source files normally; an exported archive is not an
equivalent fixture.

Before measuring, the harness verifies all of these identities:

- the historical checkout is exactly the pinned commit and contains the
  private Bash engine;
- the caller checkout is clean, and an independent candidate snapshot is
  exactly the revision stamped into the native binary and does not contain the
  private Bash engine;
- the two executables are distinct; and
- the Rust harness and binary were built with Cargo's release profile.

The driver rejects dirty source trees so every passing result is bound to one
candidate commit and tree. Candidate, historical shell, and Shdeps inputs are
run-private, detached snapshots with independently copied Git object stores,
no alternates, and read-only modes; builds use explicit manifests and targets
outside those snapshots. The driver records each commit and complete Git tree,
requires every snapshot to remain clean, and revalidates all three source
identities before each build/measurement boundary and immediately before
sealing the evidence. Changes to the caller checkout after snapshotting cannot
alter the measured source.

This is a reproducibility boundary, not a sandbox for hostile candidate code.
The gate trusts the reviewed candidate source and its build scripts plus the
exact Git, Bash, Cargo, and Rust compiler executables whose content identities
are recorded. It neutralizes external configuration and compiler wrappers.
Read-only modes prevent accidental writes, and boundary checks reject lasting
source changes, but an owner-controlled trusted build process could change a
mode, modify a snapshot, and restore it before the next check. A result must not
be treated as evidence against a deliberately malicious candidate or
toolchain. The process boundary likewise does not defend against an unrelated
same-user process that deliberately targets the run after launch; reviewed
candidate/build code and other same-user processes on the dedicated runner are
inside the trusted boundary.

## Blocking workloads and budgets

Startup and update measurements use a monotonic clock and retain nanoseconds in
the raw data. Medians use the conventional mean of the middle pair; p95 uses
the nearest-rank definition.

| Workload | Warm-up | Measured samples | Required native result |
| --- | ---: | ---: | --- |
| first native `help` spawn | none | 1 | no more than 100 ms |
| `help` | 10 per engine | 30 paired | median no more than 75% of Bash; p95 no more than 24.075 ms |
| `version` | 10 per engine | 30 paired | median no more than 75% of Bash; p95 no more than 23.85 ms |
| clean base-only update | 2 per client | 30 paired | median no more than 75% of Bash; p95 no more than 4 s |
| clean disjoint multi-overlay update | 2 per client | 30 paired | median no more than 75% of Bash; p95 no more than 1.6025 s |
| dirty disjoint multi-overlay update | shared update warm-up | 30 paired | median no more than 75% of Bash; p95 no more than 2.01 s |
| clean composed feature update | 2 per client | 30 paired | median no more than 75% of Bash; p95 no more than 12 s |
| failing pre-sync update | 2 per client | 30 paired | median no more than 75% of Bash; p95 no more than 4 s |

The startup ceilings add 125% CI headroom to the checked-in historical Rust
means of 10.7 ms for `help` and 10.6 ms for `version`. The disjoint clean and
dirty ceilings add 150% headroom to the checked-in full-update Rust p95 values
of 641 ms and 804 ms. Those reference measurements also recorded shell means
of 19.8 ms and 29.3 ms and shell full-update p95 values of 1.962 s and 2.267 s.
Every paired workload therefore independently requires the Rust median to be
at most 75% of its same-run shell median. This prevents the wider provisional
absolute ceilings for the newly composed workloads from accepting a material
relative regression. The final integrated 421-observation run must establish and
document direct absolute calibration for base-only, composed, and pre-sync
workloads before publication; this patch does not invent unavailable history.

For `help` and `version`, the semantic preflight pair is warm-up pair one; the
shared schedule executes the remaining nine pairs. Thus the recorded value of
10 warm-ups per engine exactly matches the number of commands executed before
measurement.

The update fixtures cover a base-only client, three disjoint 20-file overlays,
and a composed client with three selected overlays plus one profile-excluded
overlay. The composed client exercises the provider pinned by the current
candidate plus pre-sync and merge hooks, uses the managed-block merge API, and
has an ordered collision whose last selected overlay wins. The current
provider must retain the historical shell lock's ABI; the two lock revisions
may differ. Running the same current provider through both clients checks that
compatibility. A separate pre-sync refusal measures a representative failure
without allowing state mutation. Shell and Rust use separate HOME, XDG config,
data, state, cache, and temporary roots, but each scenario shares the same
local bare remotes. Pair order alternates on every iteration, producing 15
shell-first and 15 Rust-first samples.

Every dirty pair receives a distinct pushed commit before timing. Immediately
after each engine returns, the harness verifies that exact payload; after both
engines return, it compares normalized stdout, stderr, HOME, and all persistent
XDG config, data, state, and cache roots, plus TMPDIR. Startup commands must
also leave those roots unchanged. Snapshots include entry type, mode,
regular-file bytes, and normalized symlink targets. Exact private implementation
state is excluded narrowly: internal Dot checkouts and backups, Git metadata in
the provider checkout, the legacy Bash runtime marker, and the provider's
wall-clock freshness stamp. Dynamic engine identity fields in the shared init
receipt and client-root prefixes in private ledgers are normalized, while
semantic receipt fields and all other persistent state remain significant.
Configuration and extension inputs must also remain immutable. Only progress
rows and the completion-summary elapsed field are normalized in captured
output; diagnostic durations, counts, and versions remain significant.

Setup, mutation, validation, and statistic calculation all remain outside timed
regions. Every timed child has a 30-second deadline and an independent one MiB
limit for each stdout and stderr drain. A dedicated Linux
subreaper observes the complete descendant tree, including children that create
a new session. Process-table observations fail closed, process identities are
pinned with pidfds before signals are delivered, and a successful leader whose
descendants do not quiesce within a bounded grace period is rejected. Two empty
observations are required before a sample is accepted, and elapsed time extends
through the observation that establishes natural quiescence. The quiescence
grace is inside, not in addition to, the 30-second total command deadline.
TERM/KILL cleanup is bounded before the harness continues.

The driver selects absolute Git, Bash, Cargo, and Rust compiler executables from
documented system locations, gives measured clients a controlled PATH, and
clears ambient Git configuration. It invokes the historical launcher through
the exact selected Bash and builds both providers and Dot with an isolated
Cargo home from a config-free working directory using explicit manifest paths,
so user or ancestor Cargo configuration and compiler wrappers cannot affect the
candidate. The scratch filesystem must pass a native-execution probe before it
is used for the provider checkout or build. Uploaded artifacts record stable
location labels, strictly parsed numeric tool versions with only enumerated
public qualifiers, content identifiers, both Shdeps lock revisions and their
ABI, the controlled-path policy, and minimal filesystem type/options for the
fixture, source, and binary locations. The runner-image input is normalized
once to `local`, `other`, or a known Ubuntu label before Rust starts. Artifacts
never record arbitrary version/vendor text, absolute paths, mount points,
devices, home directories, or scratch directories.

Every Cargo build and test invocation runs under a Linux subreaper that tracks
session members, reparented descendants, and processes that detach into a new
session. While Cargo is running, the supervisor waits cheaply on the leader's
pidfd instead of repeatedly walking the host process table. It also binds its
expected driver parent by PID, start identity, and pidfd, installs a parent-death
signal with a post-installation race check, and inherits the lifecycle lock
until cleanup finishes. Handled signals remain blocked while the child enters
its stopped start barrier. A second one-byte authorization barrier linearizes a
pending cancellation against command execution before arbitrary Cargo code can
run. On leader exit, parent death, or a caught HUP, INT, QUIT, or TERM signal,
the supervisor performs stable descendant discovery, retains every opened
pidfd, and prunes exited identities. The leader remains unreaped while its
process-group identity is used; after that identity is released, cleanup
signals only retained pidfds. A Cargo command is not accepted until the
descendant tree reaches stable quiescence. A survivor, cancellation, or
process-table error fails the run, kills all safely known authority, and
performs a bounded final reap before publication can begin.
Discovered identities remain pinned by pidfd until the kernel reports their
exit; one later process-table snapshot cannot erase cleanup authority. The
timed-command supervisor uses the same monotonic rule for every identity it has
observed. The standalone build supervisor proxies command output through
stoppable readers, so loss of a previously unseen descendant cannot leave the
calling driver blocked on that descendant's inherited capture pipe.
After a command leader exits, the supervisors also consult the kernel's
direct-child inventory when that interface is available, independently pinning
newly adopted descendants before the next broad observation. If the kernel
omits that interface and the broad process view loses a new detached child
before its first pidfd and then fails permanently, exact signalling is no
longer safe. That exceptional run fails without publishing; bounded,
nonblocking capture drains let the harness return, while the child's inherited
lifecycle lock continues to exclude another publisher until the exact process
exits or is identity-checked and cleaned by an operator. The supervisor never
falls back to signalling an unverified numeric PID. A cancellation with this
incomplete cleanup returns a dedicated failure status instead of the ordinary
`128 + signal` cancellation result, and the driver preserves that status. Once
all supervised processes and output relays have finished, the supervisor
blocks the handled signals, folds both recorded and pending cancellation into
the final status, installs an `_exit`-only terminal handler, and then unblocks;
a cancellation in the final output-drain handoff therefore cannot become a
successful build.
The helper is compiled and exercised only on supported Linux `x86_64` and
`aarch64` hosts. Portable jobs retain the static driver contract and explicit
unsupported-platform rejection without attempting to build Linux-only code.

The ordered sampling and acceptance rules live in one shared `WorkloadPolicy`
table used by the harness and deterministic tests. Every paired workload keeps
the relative gate and must retain the port's material improvement, not merely
stay below a loose absolute ceiling. Every run emits an anonymized calibration
table derived from that run's accepted samples. It records ratios and budget
headroom bound to the candidate, baseline, provider, toolchain, and evidence
content identifiers; the driver independently recomputes those values before
accepting the run.

## Evidence and reproduction

Run the same Linux gate locally with:

```console
scripts/benchmark-port.sh
```

At startup the script installs cleanup, creates an opaque run nonce, and
performs every canonical-path and Git-ignore policy check—including the exact
run-specific staging and publication names—without writing to the destination
or lock directory. A rejected destination therefore leaves every byte of any
existing evidence untouched. Only after those checks pass does the driver take
an exclusive `flock` on a verified regular non-symlink file in a private stable
lock directory outside the artifact and build trees and immediately remove any
prior completion manifest before other fallible setup. The lock name is keyed
by the canonical artifact destination, so replacing that destination or its
parent cannot admit a second publisher. The driver runs its Cargo supervisor
through an interruptible wait and forwards HUP, INT, QUIT, and TERM. On exit it
closes only its own lock descriptor, without unlocking the shared open-file
description; an inherited supervisor or descendant therefore retains exclusion
until bounded cleanup is complete. Provider-build and measurement failures
retain the exact Cargo exit status and cannot publish completion evidence. The
kernel releases the lock with the final descriptor, so concurrent, killed, or
early-failed runs cannot retain a stale ownership claim or manufacture a
passing result. It recovers only its narrowly named temporary publication
directories and leaves unrelated entries in place so publication fails closed.
The benchmark writes provisional evidence in a run-private staging directory.
It publishes the durable machine-readable files to a new sibling named from
the run identity, such as
`target/performance.run-<run-id>`, and never reuses that directory. Set
`DOT_PERF_ARTIFACT_DIR` to an absolute or repository-relative publication root.
The canonical destination may not equal the source repository or be one of its
ancestors; that check happens before lifecycle-lock creation, completion
invalidation, or any destination write. Symlink aliases and normalized `.` or
`..` components do not bypass it. The destination also may not overlap the
reserved lifecycle-lock directory in either direction; its canonical path is
checked without creating that directory.
Any root that resolves inside the repository must already have Git ignore
coverage for the configured evidence directory, the hidden run-specific seal,
and the final `<root>.run-<run-id>` publication sibling. The driver verifies all
six evidence names at all three locations before creating any of them, so a
typo or overly narrow rule cannot dirty the candidate checkout. CI sets an
absolute root in the runner's temporary directory and consumes the exact
run-specific path emitted by the driver.

- `driver.tsv`: an immutable, unique-key record of source-tree identities,
  stable tool/location labels, the exact normalized Cargo test invocation and
  test name, and its zero exit status;
- `metadata.tsv`: a versioned exact schema containing revisions, profile,
  fixture shape, sanitized filesystem
  properties, tool versions, and executable content identifiers;
- `samples.tsv`: engine, workload, iteration, execution order, position,
  elapsed nanoseconds, exit status, and validation status, flushed after every
  validated pair; and
- `summary.tsv`: median, nearest-rank p95, thresholds, and pass/fail result;
- `calibration.tsv`: recomputed ratios and headroom bound to all source,
  toolchain, executable, and evidence identities; and
- `completion.tsv`: an atomically published success manifest containing the
  run identity, source/provider/toolchain identities, hashes of every other
  immutable evidence file, the exact Cargo invocation, and exact row counts.

The Rust test writes only metadata, samples, summary, and calibration into
staging. After the exact ignored test returns zero, the shell creates the one
immutable driver, independently verifies the exact five-file provisional
inventory, all identities and hashes, all 421 observations (422 physical sample
lines including the header), the eight summary rows, statistics, budgets, and
calibration arithmetic, then writes and reloads the completion manifest. It
copies the five provisional files into a private run-specific directory,
validates them again, and copies `completion.tsv` into that sealed directory
last. It validates the complete six-file set, atomically renames the directory
into its unique publication path, and validates the exact published set once
more. No later run reuses that path and no artifact write follows a successful
post-rename validation.

Immediately before upload, CI reloads the completion manifest and reruns the
complete schema, workload, statistics, identity, and content-hash validator on
that exact run-specific directory. It uploads only the six named files from
that validated path and only after the gate succeeds. Rejected staging,
mutated evidence, mixed runs, and partial publication data are never uploaded.
The gate uses local remotes, so its thresholds cover engine and local-Git cost
rather than network latency.

## Coverage boundary

The required workflow runs the complete workload inventory on every candidate,
as well as on its nightly schedule and through manual dispatch. The Shdeps
source is checked out and built at the revision already pinned by Dot before
measurement; all timed repository traffic uses local remotes. Network transfer,
fixture construction, validation, and artifact calculation are never included
in a sample. Offline runs may set `DOT_PERF_SHDEPS_SOURCE` to an existing local
clone containing that pinned revision; the benchmark still creates and checks
out its own isolated copy.

The gate makes no performance claim for dirty base-only updates, profile
transitions, large real-world dependency manifests, remote-network latency, or
every possible hook and merge API shape. Add isolated paired workloads before
making claims about those variants; do not weaken the blocking inventory or
introduce network timing to approximate them.
