# Overlays

Overlay descriptors live under `${resolved_config_home}/dot/overlays.d/`.
Git-backed and filesystem-backed overlays contribute a `home/` tree over the
base client repository. Descriptor filenames use an optional numeric ordering
prefix, such as `10-private.conf`; the remaining name selects the default
checkout `${HOME}/.dotfiles-private`.

Git-backed descriptors require `url=` and may use `platforms=`, `hosts=`, and
`optional=true|false`. Filesystem-backed descriptors use `sync=none` plus one
normalized absolute `path=` (or `~/...`) and may also use the platform and host
filters. They do not accept a URL or optional flag. Filtering is exact and
case-sensitive for platforms, case-insensitive for normalized hostnames, and
exclusions win.

Active overlays are synchronized in descriptor order. Their `home/` entries
are inventoried before mutation, the complete prospective ownership manifest
is published first, and later overlays win same-relative-path collisions.
Tracked base paths receive `skip-worktree` only while an exact managed symlink
is live. Stale links are removed only when their literal target matches durable
authority, and the base path is then restored.

Link replacement parks the exact authorized generation in a private sibling
transaction, publishes without following a late directory, revalidates the
physical parent identity, and either commits or restores the parked object.
An absent destination is always no-clobber. Recovery runs before both pre-pull
restoration and normal relinking, independent of the current descriptor set.

The generic engine does not write SSH configuration or interpret deploy-key
policy. A client that needs host aliases, keys, or other transport setup owns
that behavior in its extensions or environment before repository sync.

## Profile-aware lifecycle

When profiles are configured, profile membership is evaluated before descriptor
contents. Dot first enumerates safe descriptor filenames, maps them to logical
overlay names, and rejects ambiguous names. It opens and validates only
descriptors selected by the flattened profile; malformed unselected descriptor
contents and their transport companions are not consulted.

If no selector matches, Dot selects the client configuration's
`default_profile`; that setting defaults to `base` when omitted. A selector
always overrides the configured fallback. Changing either the fallback or the
winning selector takes effect on the next successful convergence: newly
selected overlays are activated, deselected managed links are removed, and
cached checkouts and installed packages remain available for a later upgrade.

The lifecycle uses three distinct sets:

- **selected** names come from the flattened profile;
- **eligible** records have valid selected descriptors and match the current
  platform and host, before source availability;
- **active** records have a validated synchronized Git checkout or a validated
  `sync=none` source.

Profile membership never strengthens `optional=true`. A missing key,
unreachable remote, failed optional clone, or failed optional pull remains an
advisory skip, and a later update can activate the same profile without changing
configuration. Platform/host-ineligible records expose neither component files
nor transport companions.

Convergence is two-phase. Dot reloads root configuration after pulling the base
repository, expands `base`, and passes its eligible records to pre-sync
extensions with `DOT_PRE_SYNC_STAGE=prepare`. That stage may add or refresh
supplied transport state but must preserve unmentioned managed entries. After
active phase-one overlays contribute repository-only selectors, Dot resolves
the final profile, validates all selected descriptors, and passes the final
eligible set with `DOT_PRE_SYNC_STAGE=reconcile`. Only this validated final
stage may prune entries that are no longer eligible. Merge and component-doctor
extensions receive final active records only.

Each extension worker receives its exact eligible or active records through a
private, one-use, versioned context. The worker validates and consumes that
context before client code runs; it never rediscovers descriptors, selectors,
or membership from an old installed manifest.

Command side effects are explicit:

- `init` and `update` converge both phases and may clone or pull selected
  overlays;
- `status`, `diff`, `doctor`, and `test` inspect validated existing state only;
- `fetch` fetches the root and selected existing Git checkouts without cloning
  or updating a worktree;
- `push` performs no preparatory clone, pull, or fetch and pushes only the root
  plus selected existing Git checkouts.

Changing profiles removes exact managed links during the next convergence.
Git-backed overlays may additionally provide an idempotent
`dot/profile-deactivate` hook for persistent generated state that cannot be
removed with the links themselves. Dot keeps a private lifecycle ledger so a
failed cleanup is diagnosed and retried; it validates the saved repository
identity before changing the installed link generation. Cached checkouts,
native packages, credentials, and unmanaged files are retained.
