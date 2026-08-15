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
