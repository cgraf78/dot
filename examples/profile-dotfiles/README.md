# Profile-based dotfiles

This example shows the three-profile composition model. The client repository
is always active. Profiles select additional overlays only:

- `base` selects the optional personal overlay;
- `editor` includes `base` and selects the Nvim overlay;
- `dev` includes `editor` and selects the development and optional work
  overlays.

Tracked selectors belong below `root/.config/dot/profile-selectors.d/`. A
tracked selector with only `version` and `profile` supplies a global default
without adding a new key to the base client config. Machine-local selectors
belong in the untracked
`~/.config/dot/profile-selectors.local.d/` directory and should use mode `0700`
for the directory and `0600` for each file. A successfully active phase-one
personal overlay may provide private selectors below
`dot/profile-selectors.d/`, outside its linked `home/` tree.

Selector fields are exact matches. `user` is case-sensitive. `host` is compared
after ASCII lowercasing and removal of one trailing dot. All supplied fields
must match. A selector containing both `user` and `host` is more specific than
a user-only or host-only selector and overrides broader matches. This permits a
fleet-wide compatibility default, user-wide defaults, and per-host exceptions:
the root global selector chooses `dev`, the tracked `example-user.conf` selects
`editor`, and local `combined.conf` selects `dev` for that user on
`example-host`. Multiple matches at the same specificity may agree;
conflicting choices at the same specificity fail. With no selector at all, Dot
selects the `default_profile` configured by the client, or `base` when that
setting is omitted.

All identities and URLs here are reserved examples. They do not describe a
real machine, user, or private repository.
