# dot

[![Tests](https://github.com/cgraf78/dot/actions/workflows/test.yml/badge.svg?branch=main)](https://github.com/cgraf78/dot/actions/workflows/test.yml?query=branch%3Amain)
[![Latest release](https://img.shields.io/github/release-date/cgraf78/dot?display_date=published_at&label=latest%20release)](https://github.com/cgraf78/dot/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-67ac09)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-%3E%3D1.85-blue)](Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20WSL%20%7C%20Termux-lightgrey)](#installation)

`dot` manages your dotfiles by tracking `$HOME` directly in Git. Your base
dotfiles repository's work tree is your home directory, so the base needs no
symlink farm and no copy step. Optional [overlay](docs/overlays.md)
repositories, such as a private repository on some machines or an editor
layer everywhere, link their `home/` trees on top. Per-user and per-host
[profiles](docs/configuration.md#overlay-profiles) choose which overlays a
machine gets. `dot init` shows its plan and backs up anything it would
overwrite; `dot update` syncs every repository, runs your hooks, and can
install your tools through [shdeps](https://github.com/cgraf78/shdeps). It is
a single native binary for Linux, macOS, and Android (Termux).

The engine has no configuration policy of its own: your repository supplies
the files and hooks it wants. It was built for the author's own
[dotfiles](https://github.com/cgraf78/dotfiles), which remains its main
real-world client.

How it compares:

- [yadm](https://yadm.io/) also makes `$HOME` the work tree of a Git
  repository. Where yadm handles per-machine differences with alternate files
  and templates inside one repository, `dot` composes several repositories
  and adds versioned hook APIs and `dot doctor` health checks.
- [chezmoi](https://www.chezmoi.io/) and
  [GNU Stow](https://www.gnu.org/software/stow/) keep a separate source tree
  and copy or symlink it into place. `dot` skips that step for the base
  repository; only overlays are linked.

## Installation

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh | bash
```

The installer needs `curl`, `tar`, and `sha256sum` or `shasum`, and runs on
stock macOS Bash 3.2. It verifies
the release's published SHA-256 checksum, plus its GitHub artifact
attestation when a logged-in `gh` 2.49 or newer is available (see
[security.md](docs/security.md)). It links `dot` into `~/.local/bin`
(`$PREFIX/bin` on Termux), so make sure that directory is on your `PATH`. The
engine itself needs no shell; user hooks and shdeps integration need Bash 4
or newer.

For manual archive verification, follow the shared
[attestation verification guide](https://github.com/cgraf78/actions/blob/main/docs/workflow-api.md#verifying-published-attestations).
The release source is `cgraf78/dot`; its signer is the reusable release
workflow in `cgraf78/actions`.

To initialize a client in the same operation:

```bash
curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh |
  bash -s -- --init https://github.com/example/dotfiles.git
```

[installation.md](docs/installation.md) covers the published layout, its
safety checks, and installs managed by Shdeps.

## Try it

A client can be as small as one tracked `.bashrc`; see
[examples/minimal-dotfiles](examples/minimal-dotfiles/). Point `dot` at your
own repository, then use the daily commands:

```bash
dot init https://github.com/<you>/dotfiles.git
dot status   # show base and overlay status
dot update   # sync repositories, run hooks, run shdeps if configured
dot push     # push the base repository and active Git overlays
dot doctor   # run health checks
```

`dot init` lists any existing files the repository would replace and asks
before replacing them (pass `--yes` to skip the prompt); replaced files go to
`~/.dot-backup/`. Run `dot help` for every command and `dot init --help` for
recovery options.

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

`dot doctor` runs built-in health checks plus configured `doctor.d` extensions
(`dot doctor --help` lists its environment knobs).
`dot test` runs trusted executable test extensions from the configured `tests`
directory. The provider-owned `dot` suite remains visible in `dot test --list`
and can be selected explicitly with `dot test dot`; set
`DOT_TEST_INCLUDE_PROVIDER=1` to include it in an unfiltered run. Other names
select exact or prefix subsets.

## Development

```bash
cargo build --locked
cargo test --locked
bash tests/run
bash tests/release-scripts-test
```

`cargo test --locked` runs the Rust unit and integration suites. `tests/run`
drives the native binary through `bin/dot`, which requires a prior
`cargo build --locked`. The provider entry point runs its independent test
files concurrently through the same bounded coordinator used by `dot test`.
`tests/release-scripts-test` covers the release, packaging, and benchmark
scripts; `tests/run` skips it, so CI runs it separately.

The project uses the shared Rust Linux, macOS, musl, Android, and Termux matrix,
plus a shell matrix for the installer and public hook APIs.

For release preparation, see [AGENTS.md](AGENTS.md#release), including the
shared verification procedure for published artifacts.

Licensed under the [MIT License](LICENSE).
