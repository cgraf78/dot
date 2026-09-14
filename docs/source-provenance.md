# Source provenance

This repository has fresh history. Paths adapted from the public
`cgraf78/dotfiles` tree are attributed to immutable named revisions recorded in
[`source-revisions-v1.tsv`](source-revisions-v1.tsv). New
standalone-only seams are marked as such below rather than being attributed to
an older file.

## Generated and shared inputs

| Standalone path | Public source or origin | Notes |
| --- | --- | --- |
| `install.sh` | `cgraf78/actions` release installer at the revision in `.github/cgraf78-actions.lock` | Generated, never hand-edited |
| `support/shdeps.lock` | Its recorded immutable `cgraf78/shdeps` revision | Pins revision, installer digest, and ABI without duplicating that revision in this inventory |
| `docs/source-revisions-v1.tsv` | Reviewed immutable public extraction inputs | Gives each source revision one stable name so per-path provenance does not duplicate commit hashes |

## Native runtime implementation

The CLI and engine are implemented by `src/*.rs`. Their behavior was ported
from the public `cgraf78/dotfiles` inputs named in
[`source-revisions-v1.tsv`](source-revisions-v1.tsv), then split into native
modules by responsibility. The removed private `lib/dot/*.sh` tree is not a
runtime or test dependency.

## Retained public shell boundary

| Standalone path | Public source or origin | Extraction notes |
| --- | --- | --- |
| `lib/dot/public/xdg.sh` | `dotfiles:.local/lib/dot/core/xdg.sh` | Public names, strict argument validation, and API inventory added |
| `lib/dot/public/ui.sh` | `dotfiles:.local/lib/dot/core/ui.sh` | Public names, deterministic caller-state behavior, and API inventory added |
| `lib/dot/public/{api-version.sh,api-v1.tsv,variables-v1.tsv}` | New standalone interface | Machine-readable public ABI and drift checks |
| `lib/dot/public/hook-runtime-v1/**` | Reviewed extraction of the former hook API helpers | Self-contained compatibility surface sourced only by user-provided hooks; it cannot dispatch the CLI or engine |
| `lib/dot/public/{doctor-api-v1.tsv,hook-api-v1.tsv,test-api-v1.tsv}` | New standalone interfaces | Machine-readable public boundary inventories |
| `lib/dot/public/test-timeout-v1` | `dotfiles:.local/lib/dotfiles/tests/timeout.py` at `dotfiles-v2` | Versioned portable timeout command for suites and provider-owned descendant cleanup |
| `lib/dot/public/test-reporter-v1` | New standalone interface | Language-neutral, single-terminal-record result transport for executable suites |
| `support/client-launcher.sh` | New standalone release boundary | Installs or invokes a verified native release artifact; it is not an engine fallback |

## Test provenance

| Standalone tests | Public source or origin | Notes |
| --- | --- | --- |
| `tests/repos_*.rs`, `tests/reserved.rs`, and `tests/update_lock.rs` | Generic cases extracted from `dotfiles:.local/lib/dot/tests/{core-pull-test,core-overlays-test,core-update-test,core-resource-cleanup-test,xdg-test}` and `tests/core/commands.sh` | Expanded with standalone topology, reserved-path, cancellation, and crash-phase fixtures; repository and update-lock contracts run directly against native owners |
| `tests/{families.rs,hook_api.rs,extension_worker.rs,extension_trust.rs,overlay_context.rs}` and `tests/hooks-test` | Generic cases extracted from `core-merges-test`, `core-resource-cleanup-test`, and `tests/core/merges.sh` | Concrete application hook cases deliberately excluded; family selection, API inventories, trust/context matrices, and hook contracts run directly against native owners, with hermetic public-runtime and user-hook Bash boundary tests |
| `tests/{shdeps.rs,shdeps_checkpoint.rs,shdeps_env_abi.rs,shdeps_provider.rs,shdeps_ui.rs,shdeps_ui_render.rs}` | Generic cases extracted from `core-doctor-test`, `core-test`, `core-reexec-test`, and provider/UI portions of the public suite | Lock, checkpoint, environment ABI, provider coordination, progress records, rendering, lifecycle, and failure contracts run directly against native owners with fake external Shdeps binaries only at the public provider boundary |
| `tests/{cli.rs,config.rs,profiles.rs}` and `tests/{init-test,client-launcher-test,library-test,workflow-test}` | New standalone acceptance suites, informed by public bootstrap/launcher/XDG/workflow characterization | Use only synthetic users, repositories, hosts, and paths; CLI, configuration, profile, and checked-in example contracts now run directly against the native engine |
| `tests/test-lifecycle-test` | Generic cases extracted from `dotfiles:.local/lib/dotfiles/tests/core/runner.sh` at `dotfiles-v2` | Covers filtering, priority, stdin closure, timeout, descendant cleanup, concurrent temp ownership, and CLI failures |
| `tests/lib/{test.sh,repo.sh}`, `tests/run`, and `tests/provider-suite-wrapper` | New standalone harness, adapted from public dotfiles test conventions | Checkout-local only; delegates provider files to the shared parallel coordinator and does not invoke client-owned tests |

`tests/provenance-test` validates the revision and per-path inventories. It
requires named full commit IDs in the revision inventory, compares the manifest
with the tracked `install.sh`, `bin`, `lib/dot`, `support`, and `tests` files,
verifies each listed path and accepted origin form, and keeps full revisions out
of the per-path manifest. The test is limited to these inventories; other
repository content and generated-artifact freshness are outside its contract.

The exact one-row-per-path inventory used by that gate is
[`source-provenance-v1.tsv`](source-provenance-v1.tsv). Any added, removed, or
renamed implementation/test path must update the inventory and its reviewed
origin in the same change.
