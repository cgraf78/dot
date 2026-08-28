# Configuration

Dot reads `${resolved_config_home}/dot/config`, where a relative or empty
`XDG_CONFIG_HOME` falls back to `$HOME/.config`. The file is never evaluated as
shell code.

Example:

```text
version=1
extension_api=1
extensions_dir=$HOME/.local/lib/dotfiles
dependency_provider=shdeps
default_profile=dev
shdeps_update_policy=pinned
```

The first non-comment setting must be `version=1`. Keys may appear only once.
Supported keys are:

| Key | Values |
| --- | --- |
| `version` | `1` |
| `extension_api` | `1` |
| `extensions_dir` | Normalized absolute path after a leading `~`, `$HOME`, or `${HOME}` expansion |
| `dependency_provider` | `none` or `shdeps` |
| `default_profile` | Lowercase profile name (default: `base`) |
| `shdeps_update_policy` | `pinned` or `latest` (default: `pinned`) |

`extensions_dir` requires `extension_api=1`. Unknown keys, control bytes,
continuations, duplicate keys, unsupported versions, and other variable
expansions fail before any provider or extension executes. The config must be
a regular non-symlink file no larger than 65,536 bytes. A HOME-based value
requires `HOME` itself to be a normalized absolute path; mixed expansion tokens
such as `$HOME/path/$OTHER` are rejected rather than partially expanded.

## Shdeps update policy

`pinned` preserves Dot's immutable provider boundary. A development checkout at
`${SHDEPS_GIT_DEV_DIR:-$HOME/git}/shdeps` is selected only when its revision and
installer digest match `support/shdeps.lock`; otherwise Dot uses a matching
managed install or downloads the installer from the locked revision.

`latest` opts the client into checking for the newest Shdeps on every update.
A local development checkout is accepted across revision changes only when its
root, bootstrap entrypoints, and Git metadata pass ownership and mode checks,
and both its recorded and effective origins identify `cgraf78/shdeps` on
GitHub. Selecting it is an explicit trust decision: Dot then treats the whole
user-controlled checkout as executable developer input, including existing
binaries and Cargo build inputs; the identity checks are not a recursive
content sandbox. Shdeps checks and updates that checkout and rebuilds its
binary when the checked-out revision changes. Without a valid development
checkout, Dot keeps the locked installer digest as its bootstrap trust anchor
and forces the managed release path to check for and activate the newest
available release. That force is scoped to provider bootstrap: ordinary
dependency convergence still uses Shdeps' normal cache and update policy unless
the caller explicitly requests `dot update --force` or sets `SHDEPS_FORCE`.
Use `pinned` unless you control and trust the local checkout.

Set `DOT_SHDEPS_UPDATE_POLICY` to `pinned` or `latest` for a process-local
override; the environment value takes precedence over the config file. Invalid
config or environment values fail before any provider or extension runs. A
network or metadata failure retains an already-compatible managed release when
Shdeps can do so safely; if no usable provider can be activated, `dot update`
returns failure.

Maintainers update `support/shdeps.lock` by selecting a reviewed immutable
revision and recording the SHA-256 digest of that revision's raw `install.sh`.
`scripts/verify-shdeps-lock` checks the canonical lock schema and fetches the
raw installer at that exact revision, proving both that the revision and path
exist and that the bytes match the recorded digest. CI always reports one
singleton lock check, but performs that bounded remote verification only when
the lock changes (or for a manual run); it never selects or advances the
revision automatically.

## Overlay profiles

Clients may define additive overlay profiles in
`${resolved_config_home}/dot/profiles.d/<name>.conf`:

```text
version=1
profiles=base
overlays=nvim
```

`profiles=` and `overlays=` are comma-separated lowercase identifiers. A
profile may include other profiles, but cycles and unknown parents are errors.
Included profiles are expanded before the including profile, and duplicate
overlay names are removed without changing descriptor order. Every flattened
profile must select at least one overlay. The client/root repository is always
active and must not appear in `overlays=`.

When `profiles.d` exists, it must define `base`. With no matching selector, Dot
selects `default_profile` from the client configuration; omitting that setting
selects `base`. The configured name must identify a defined profile. When
`profiles.d` does not exist, Dot retains its legacy behavior and considers every
overlay descriptor.

Selectors use strict data files with this schema:

```text
version=1
user=example-user
host=example-host
profile=editor
```

At least one of `user` or `host` is required, and every supplied field must
match. User names come from `id -un` and compare exactly and case-sensitively.
Short hostnames come from `hostname -s`; both configured and current values are
ASCII-lowercased after removing one trailing dot. A selector containing both
`user` and `host` is more specific than a selector containing only one field,
and the most-specific matching level wins. This permits a user-wide default
with per-host exceptions. Multiple matches at the winning specificity may
agree on a profile; conflicting choices at that same specificity are a
configuration error. Less-specific disagreements are ignored.

Selector sources are read in this order; source location does not affect
precedence:

1. tracked root files in `.config/dot/profile-selectors.d/`;
2. untracked machine-local files in
   `.config/dot/profile-selectors.local.d/`;
3. repository-only `dot/profile-selectors.d/` files from active overlays
   selected by `base`.

Machine-local selector directories and files must be owned by the current user,
must not be symlinks, and must have no group/other permission bits. Use `0700`
for the directory and `0600` for files. Client repositories should ignore the
exact path `.config/dot/profile-selectors.local.d/`; Dot also rejects that path
from every base or overlay candidate. Overlays cannot publish linked
`profiles.d` or `profile-selectors.d` paths.

Profile selection has no environment-variable override and no mutation command.
See the executable, sanitized scenarios in
[`examples/profile-dotfiles/`](../examples/profile-dotfiles/).
