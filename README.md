# dot

`dot` is a reusable dotfiles convergence engine. It manages a client Git
repository whose work tree is your home directory, optional overlay
repositories, versioned extension hooks, and an optional dependency provider.
The engine contains no application-specific configuration policy: the client
repository supplies the files and extensions it wants.

## Installation

Install the checkout-backed release with:

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh | bash
```

To initialize a client in the same operation:

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh |
  bash -s -- --init https://github.com/example/dotfiles.git
```

The installer and public launcher run on stock macOS Bash 3.2, locate a
validated Bash 4+ runtime, and use the same canonical checkout as Shdeps:
`${SHDEPS_INSTALL_DIR:-$HOME/.local/share}/cgraf78/dot`. The installer never
creates a second XDG-specific checkout.

It publishes:

- `~/.local/bin/dot` -> `<checkout>/bin/dot`
- `~/.local/lib/dot` -> `<checkout>/lib/dot/public`

Both destinations are no-clobber. A client repository may retain a regular
`~/.local/bin/dot` only when it is byte-identical to the generated permanent
front door in `support/client-launcher.sh`. That front door derives the same
official install root, requires `~/.local/lib/dot` to resolve to that checkout's
public library, and then executes its standalone runtime without sourcing
client or checkout code itself. Missing topology reports the reinstall command.

Other regular files and directories are rejected throughout.

## Runtime model

The dot tool checkout is an ordinary repository. The selected client dotfiles
repository is separate: fresh initialization uses `~/.dotfiles` as its Git
directory with an explicit absolute `core.worktree=$HOME`. Existing legacy
bare clients and identified ordinary checkouts rooted at `$HOME` remain
supported.

If the separate client Git directory is lost, remove or move aside
`~/.dotfiles` and rerun the same `dot init` command. When no ordinary
`$HOME/.git` repository is present, Dot retires the matching completed record,
backs up existing worktree paths, and rebuilds a fresh client generation.

`dot` shadows Graphviz's command of the same name when `~/.local/bin` precedes
the system path. Invoke Graphviz by its explicit system path when both tools
are installed.

## Configuration and extensions

Configuration lives at `${XDG_CONFIG_HOME:-$HOME/.config}/dot/config` and is
strict data, not sourced shell. A missing file enables no provider and no
extensions. See [configuration.md](docs/configuration.md) and
[extensions.md](docs/extensions.md).

Clients using Shdeps can keep Dot's default immutable provider selection with
`shdeps_update_policy=pinned`, or opt into a freshness check on every update
with `shdeps_update_policy=latest`. Latest mode follows a validated local
`cgraf78/shdeps` development checkout when present, treating that user-owned
checkout's contents as trusted executable developer input. Otherwise it
refreshes the managed release through Dot's pinned bootstrap trust anchor.
This freshness check does not force every configured dependency to be checked;
use `dot update --force` when dependency-wide forced convergence is intended.

Only the versioned modules under `lib/dot/public` are sourceable APIs. All
other shell files are private runtime implementation. See
[library.md](docs/library.md).

`dot doctor` runs built-in health checks plus configured `doctor.d` extensions.
`dot test` runs trusted executable test extensions from the configured `tests`
directory. The provider-owned `dot` suite remains visible in `dot test --list`
and can be selected explicitly with `dot test dot`; set
`DOT_TEST_INCLUDE_PROVIDER=1` to include it in an unfiltered run. Other names
select exact or prefix subsets.

## Development

```bash
tests/run
```

The provider entry point runs its independent test files concurrently through
the same bounded coordinator used by `dot test`.

The project uses the full shared Linux, macOS, and Termux shell matrix plus a
stock macOS Bash 3.2 bootstrap job.

Licensed under the [MIT License](LICENSE).
