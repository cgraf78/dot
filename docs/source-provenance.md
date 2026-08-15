# Source provenance

This repository has fresh history. Every imported implementation file must be
traceable to the public base dotfiles engine; no file from any private overlay
repository may seed this repository.

| Standalone path | Public source or origin | Notes |
| --- | --- | --- |
| `lib/dot/public/xdg.sh` | `dotfiles:.local/lib/dot/core/xdg.sh` | Public names and API contract added during extraction |
| `lib/dot/public/ui.sh` | `dotfiles:.local/lib/dot/core/ui.sh` | Public names and API contract added during extraction |
| `install.sh` | `actions:checkout-installer` at `.github/cgraf78-actions.lock` | Generated, never hand-edited |
| `support/checkout-bash-v1.sh` | `actions:checkout-installer/bash-resolver-v1.sh.in` | Generated from the same reviewed source as `install.sh` |
| All other skeleton files | New standalone implementation | Written for the public policy-free interface |

The table expands as engine modules move. Before first public visibility, CI
and fresh-eyes review compare it with the complete tracked source inventory and
run a repository-wide work/private deny-list scan.
