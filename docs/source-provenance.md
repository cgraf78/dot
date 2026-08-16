# Source provenance

This repository has fresh history. Public engine extraction used only the
public `cgraf78/dotfiles` tree at commit
`1f178787fbb7fc4dfdacbabea01fc196cf6aa462`; no implementation or fixture was
copied from a private overlay repository. New standalone-only seams are marked
as such below rather than being attributed to an older file.

## Generated and shared inputs

| Standalone path | Public source or origin | Notes |
| --- | --- | --- |
| `install.sh` | `cgraf78/actions` checkout-installer at the revision in `.github/cgraf78-actions.lock` | Generated, never hand-edited |
| `support/checkout-bash-v1.sh` | `actions:checkout-installer/bash-resolver-v1.sh.in` at the same locked revision | Generated from the resolver embedded in `install.sh` |
| `support/shdeps.lock` | Its recorded immutable `cgraf78/shdeps` revision | Pins revision, installer digest, and ABI without duplicating that revision in this inventory |

## Public runtime implementation

| Standalone path | Public source or origin | Extraction notes |
| --- | --- | --- |
| `lib/dot/public/xdg.sh` | `dotfiles:.local/lib/dot/core/xdg.sh` | Public names, strict argument validation, and API inventory added |
| `lib/dot/public/ui.sh` | `dotfiles:.local/lib/dot/core/ui.sh` | Public names, deterministic caller-state behavior, and API inventory added |
| `lib/dot/public/{api-version.sh,api-v1.tsv,variables-v1.tsv}` | New standalone interface | Machine-readable public ABI and drift checks |
| `lib/dot/{log.sh,progress-ui.sh,resources.sh,run.sh,temp.sh,update-lock.sh}` | Same-named files under `dotfiles:.local/lib/dot/core/` | Generic runtime and cleanup behavior extracted; client policy removed |
| `lib/dot/constants.sh` | `dotfiles:.local/lib/dot/core/constants.sh` | Reduced to standalone/XDG runtime state |
| `lib/dot/platform.sh` | `dotfiles:.local/lib/dot/core/platform.sh` | Only generic platform and host filtering retained |
| `lib/dot/pre-sync.sh` | New standalone extension lifecycle | Runs trusted client prerequisites before repository network or checkout mutation |
| `lib/dot/overlays.sh` | `dotfiles:.local/lib/dot/core/overlays.sh` | Generic descriptor and local-source validation retained; SSH application policy removed |
| `lib/dot/repos/{api.sh,commands.sh,config.sh,dirty.sh,git.sh,overlays.sh,pull.sh}` | Same-named files under `dotfiles:.local/lib/dot/core/repos/` | URL rewriting, personal branch assumptions, and application policy removed |
| `lib/dot/repos/model.sh` | New standalone seam, adapted from `core/constants.sh`, `core/init.sh`, and repo helpers | Typed ordinary/separate-Git-dir topology and repository identity |
| `lib/dot/{families.sh,merge-block.sh,merge-hooks.sh,merges.sh}` | Same-named files under `dotfiles:.local/lib/dot/core/` | Concrete application hooks removed; generic ordering, publication, and scheduling retained |
| `lib/dot/{extension-trust.sh,extension-worker-launch.sh,extension-worker.sh}` | New standalone split from `core/merges.sh` and `core/merge-hooks.sh` | Shared trust validation and fresh-Bash worker boundary |
| `lib/dot/{hook-api.sh,hook-api-v1.tsv}` | New standalone interface, using generic helpers formerly in `core/merge-hooks.sh` | Versioned extension API and drift inventory |
| `lib/dot/providers/shdeps-ui.sh` | `dotfiles:.local/lib/dot/core/shdeps-ui.sh` | Generic JSONL rendering and cancellation retained |
| `lib/dot/providers/shdeps.sh` | Adapted from `core/init.sh`, `core/update.sh`, and `core/shdeps-assets.sh` | Immutable pin, optional provider, and one-shot standalone re-exec |
| `lib/dot/doctor.sh` | `dotfiles:.local/lib/dot/core/doctor.sh` | Core-first coordinator plus isolated client extension stage |
| `lib/dot/doctor/{runtime.sh,paths.sh,repos.sh,overlays.sh,merges.sh}` | Same-named files under `dotfiles:.local/lib/dot/core/doctor/` | Only generic runtime/repository/overlay/hook checks retained |
| `lib/dot/doctor/{lock.sh,provider.sh}` | New standalone checks derived from update-lock and provider contracts | No application-specific diagnostics |
| `lib/dot/{doctor-api.sh,doctor-api-v1.tsv}` | New standalone interface | Isolated result transport and machine-readable ABI |
| `lib/dot/init-client.sh` | New standalone transaction, adapted from topology and bootstrap behavior in `core/init.sh` | Candidate quarantine, durable identity, rollback, and forward convergence |
| `lib/dot/reserved.sh` | New standalone security seam | Dynamic Actions/Shdeps/provider/client control-plane inventory |
| `lib/dot/update.sh` | `dotfiles:.local/lib/dot/core/update.sh` | Provider-neutral orchestration; client integrations removed |
| `lib/dot/{commands.sh,init.sh,main.sh,runtime.sh,config.sh}` | New standalone composition around the extracted public engine | CLI, strict config, loading order, and product/version boundary |

## Test provenance

| Standalone tests | Public source or origin | Notes |
| --- | --- | --- |
| `tests/{repos-test,resources-test,update-lock-test}` | Generic cases extracted from `dotfiles:.local/lib/dot/tests/{core-pull-test,core-overlays-test,core-update-test,core-resource-cleanup-test,xdg-test}` and `tests/core/commands.sh` | Expanded with standalone topology, reserved-path, cancellation, and crash-phase fixtures |
| `tests/{families-test,merge-block-test,hooks-test,hook-api-test,extensions-api-test}` | Generic cases extracted from `core-merges-test`, `core-resource-cleanup-test`, and `tests/core/merges.sh` | Concrete application hook cases deliberately excluded |
| `tests/{doctor-test,shdeps-provider-test}` | Generic cases extracted from `core-doctor-test`, `core-test`, `core-reexec-test`, and provider/UI portions of the public suite | Application and environment checks remain client-owned |
| `tests/{init-test,cli-test,config-test,install-test,client-launcher-test,library-test,examples-test,workflow-test}` | New standalone acceptance suites, informed by public bootstrap/launcher/XDG/workflow characterization | Use only synthetic users, repositories, hosts, and paths |
| `tests/lib/{test.sh,repo.sh}` and `tests/run` | New standalone harness, adapted from public dotfiles test conventions | Checkout-local only; never invokes the private/client suite |

Before first public visibility, CI compares this inventory with the tracked
standalone implementation and test inventory, runs a repository-wide
work/private deny-list scan, and verifies that every generated artifact still
matches its pinned public provider revision.

The exact one-row-per-path inventory used by that gate is
[`source-provenance-v1.tsv`](source-provenance-v1.tsv). Any added, removed, or
renamed implementation/test path must update the inventory and its reviewed
origin in the same change.
