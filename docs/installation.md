# Installation

The [README](../README.md#installation) has the install commands. This page
covers what the installer verifies and publishes, and how it coexists with a
client that manages Dot through Shdeps.

## Verification

Online installation requires `curl`. When the GitHub CLI (`gh`) 2.49 or newer
is installed and logged in to github.com, the installer also verifies the
downloaded archive's GitHub artifact attestation before it is activated and
fails if that verification fails for any reason, including a missing
attestation or an expired credential. Otherwise, as on most fresh hosts, it
installs on the published SHA-256 checksum alone and says so; pass
`--require-attestation` to fail instead. See [security.md](security.md).

To verify a downloaded archive manually, use the shared
[attestation verification guide](https://github.com/cgraf78/actions/blob/main/docs/workflow-api.md#verifying-published-attestations)
with source repository `cgraf78/dot` and the pinned shared release workflow
in `cgraf78/actions` as signer.

## Layout

The installer runs on stock macOS Bash 3.2, downloads the platform archive,
verifies its published checksum (plus its signer-aware attestation from
`cgraf78/actions` when `gh` can check it), and atomically selects a versioned
release under
`${XDG_DATA_HOME:-$HOME/.local/share}/cgraf78`. The `dot` engine is a native
executable and does not require Bash. Bash 4 or newer is resolved for configured
shell capabilities: when a user hook or the reviewed Shdeps bootstrap must run,
and when `dot doctor` validates either capability.

It publishes the stable links:

- `~/.local/bin/dot` -> `<data-home>/cgraf78/dot/dot`
- `<data-home>/cgraf78/dot` -> the current immutable release directory

Both destinations are fail-closed around foreign content. Existing client
repositories may retain a regular `~/.local/bin/dot` only when it is
byte-identical to the compatibility adapter in `support/client-launcher.sh`.
That adapter resolves the same standalone release root and executes its native
binary without sourcing client code. Missing topology reports the reinstall
command.

The installer rejects other regular files and directories throughout.

## Installs managed by Shdeps

A client that instead manages Dot as a Shdeps `github:release` dependency gets
`<data-home>/cgraf78/dot` as a plain release directory maintained by Shdeps
rather than a stable link to a versioned release. The native runtime and
`dot doctor` accept that layout because they identify a packaged release root
by its `.dot-install.json` metadata, not by the link. This installer does not:
rerunning it on such a host fails because the stable release root is a
user-owned directory, so keep updating Dot through Shdeps there.

A host first bootstrapped with this installer and later managed by Shdeps
converges to that layout on its own: on Shdeps' next update of Dot, even when
the installed release is already current, Shdeps verifies the installer's own
layout and adopts the stable root in place (so a client's Shdeps `post` hook
for Dot can publish `~/.local/lib/dot` right away), keeping the published `dot`
command link working throughout. The private `.dot-standalone` directory is
left behind, inert, and is safe to delete afterwards. This needs a Shdeps
release with standalone adoption; once Shdeps has recorded Dot, older Shdeps
fail closed with "release asset format changed; manual layout migration is
required" until Shdeps itself updates.

`dot doctor` reports which installer owns the release Shdeps would update.
Under the Shdeps provider a standalone install warns until Shdeps adopts it on
its next update of Dot (if it persists, `shdeps health` says why), an
interrupted adoption (`dot.shdeps-parked-root`) warns until the next update
finishes it, and a leftover `.dot-standalone/lock` fails, because Shdeps will
not adopt the install while it exists. A Shdeps release root whose
`.shdeps-release-layout` marker is not exactly `v1 archive` fails (a missing
marker warns; Shdeps backfills it when the public command proves ownership),
and an interrupted Shdeps install's `dot.shdeps-archive-backup-*` sibling
warns. Without a provider, the standalone installer is the upgrade path and
its lock only warns.
