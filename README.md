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

Both destinations are no-clobber. An existing regular `dot` command is
preserved only when it is byte-identical to the generated client adapter in
`support/client-launcher.sh`; other regular files and directories are rejected.
The optional adapter reads a client-owned phased cutover lock. The `prepare`
phase always uses the retained embedded launcher while the client installs and
validates standalone Dot. The `active` phase additionally requires a private,
revision-keyed host-readiness directory and proves that the lock's minimum
revision is an ancestor of the active checkout. Re-execs inherited from the
embedded updater also stay on that launcher because its private arguments are
not standalone API. Missing or malformed activation authority denies the
standalone path but does not interrupt an available embedded fallback. Before
any embedded fallback, the adapter asks the
client-owned handoff helper to restore a recoverable legacy tree. After the
restoration horizon it keeps the same bytes and prints the recovery installer
when neither runtime is available.

## Runtime model

The dot tool checkout is an ordinary repository. The selected client dotfiles
repository is separate: fresh initialization uses `~/.dotfiles` as its Git
directory with an explicit absolute `core.worktree=$HOME`. Existing legacy
bare clients and identified ordinary checkouts rooted at `$HOME` remain
supported.

`dot` shadows Graphviz's command of the same name when `~/.local/bin` precedes
the system path. Invoke Graphviz by its explicit system path when both tools
are installed.

## Configuration and extensions

Configuration lives at `${XDG_CONFIG_HOME:-$HOME/.config}/dot/config` and is
strict data, not sourced shell. A missing file enables no provider and no
extensions. See [configuration.md](docs/configuration.md) and
[extensions.md](docs/extensions.md).

Only the versioned modules under `lib/dot/public` are sourceable APIs. All
other shell files are private runtime implementation. See
[library.md](docs/library.md).

## Development

```bash
tests/run
```

The project uses the full shared Linux, macOS, and Termux shell matrix plus a
stock macOS Bash 3.2 bootstrap job.

Licensed under the [MIT License](LICENSE).
