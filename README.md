# dot

`dot` is a reusable dotfiles convergence engine. It manages a client Git
repository whose work tree is your home directory, optional overlay
repositories, versioned extension hooks, and an optional dependency provider.
The engine contains no application-specific configuration policy: the client
repository supplies the files and extensions it wants.

## Installation

Install the native release with:

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh | bash
```

Online installation requires both `curl` and the GitHub CLI (`gh`). The latter
verifies the downloaded archive's GitHub artifact attestation before it is
activated.

To initialize a client in the same operation:

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh |
  bash -s -- --init https://github.com/example/dotfiles.git
```

The installer runs on stock macOS Bash 3.2, downloads the platform archive,
verifies its published checksum and signer-aware attestation from
`cgraf78/actions`, and atomically selects a versioned release under
`${XDG_DATA_HOME:-$HOME/.local/share}/cgraf78`. The `dot` engine is a native
executable and does not require Bash. Bash 4 or newer is needed only when a
configured user hook uses the versioned shell extension API.

It publishes the stable links:

- `~/.local/bin/dot` -> `<data-home>/cgraf78/dot/dot`
- `<data-home>/cgraf78/dot` -> the current immutable release directory

Both destinations are fail-closed around foreign content. Existing client
repositories may retain a regular `~/.local/bin/dot` only when it is
byte-identical to the compatibility adapter in `support/client-launcher.sh`.
That adapter resolves the same standalone release root and executes its native
binary without sourcing client code. Missing topology reports the reinstall
command.

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
extensions. Optional profile definitions select additive overlay sets by exact
user/host records; the root client remains always active. With no matching
selector, `default_profile` chooses the configured fallback and defaults to
`base` when omitted. See [configuration.md](docs/configuration.md),
[overlays.md](docs/overlays.md), the sanitized
[profile examples](examples/profile-dotfiles/), and
[extensions.md](docs/extensions.md).

Clients using Shdeps can keep Dot's default immutable provider selection with
`shdeps_update_policy=pinned`, or opt into a freshness check on every update
with `shdeps_update_policy=latest`. Latest mode follows a validated local
`cgraf78/shdeps` development checkout when present, treating that user-owned
checkout's contents as trusted executable developer input. Otherwise it
refreshes the managed release through Dot's pinned bootstrap trust anchor.
This freshness check does not force every configured dependency to be checked;
use `dot update --force` when dependency-wide forced convergence is intended.

Only the versioned modules under `lib/dot/public` are sourceable APIs. They are
the shell boundary for user-authored hooks, not an alternate implementation of
the engine. See [library.md](docs/library.md).

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

The project uses the shared Rust Linux, macOS, musl, Android, and Termux matrix,
plus a shell matrix for the installer and public hook APIs.

Licensed under the [MIT License](LICENSE).
