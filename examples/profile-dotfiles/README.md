# Profile-based dotfiles

This example shows the three-profile composition model. The client repository
is always active. Profiles select additional overlays only:

- `base` selects the optional personal overlay;
- `editor` includes `base` and selects the Nvim overlay;
- `dev` includes `editor` and selects the development and optional work
  overlays.

Tracked selectors belong below `root/.config/dot/profile-selectors.d/`.
Machine-local selectors belong in the untracked
`~/.config/dot/profile-selectors.local.d/` directory and should use mode `0700`
for the directory and `0600` for each file. A successfully active phase-one
personal overlay may provide private selectors below
`dot/profile-selectors.d/`, outside its linked `home/` tree.

Selector fields are exact matches. `user` is case-sensitive. `host` is compared
after ASCII lowercasing and removal of one trailing dot. All supplied fields
must match. Multiple matches may agree; conflicting profile choices fail.
With no match, Dot selects `base`.

All identities and URLs here are reserved examples. They do not describe a
real machine, user, or private repository.
