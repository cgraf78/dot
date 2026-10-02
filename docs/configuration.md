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

The first non-comment setting must be `version=1`. Each supported key may
appear only once. Supported keys are:

| Key | Values |
| --- | --- |
| `version` | `1` |
| `extension_api` | `1` |
| `extensions_dir` | Normalized absolute path after a leading `~`, `$HOME`, or `${HOME}` expansion |
| `dependency_provider` | `none` or `shdeps` |
| `default_profile` | Lowercase profile name (default: `base`) |
| `shdeps_update_policy` | `pinned` or `latest` (default: `pinned`) |

`extensions_dir` requires `extension_api=1`. Control bytes, continuations,
malformed lines or key names, duplicate supported keys, invalid values,
unsupported versions, and other variable expansions fail before any provider or
extension executes. The config must be a regular non-symlink file no larger
than 65,536 bytes. A HOME-based value requires `HOME` itself to be a normalized
absolute path; mixed expansion tokens such as `$HOME/path/$OTHER` are rejected
rather than partially expanded.

## Unknown keys and version skew

The config file usually travels through the client repository, and the client
repository and Dot update independently. A well-formed key (`[a-z_]+`) that
this Dot does not know is therefore ignored, not rejected, so a client
repository that adopts a newer key cannot stop an older Dot from converging and
upgrading itself. Its value is never read, and repeating it is not an error,
but its line must still follow the file-wide rules: one `key=value` line with
no continuation or control bytes. Every rule for the keys above still applies,
including the `version` check and the rule that `version=1` comes first.

Each operational command prints one warning per ignored key on stderr before
it runs:

```text
dot: config: warning: unknown key 'future_key' ignored (newer dot?)
dot: config: warning: unknown key 'defualt_profile' ignored (did you mean 'default_profile'?)
```

The second form appears when the key is within two edits of a known key. A
misspelled key warns instead of failing, so its setting keeps its default until
the spelling is fixed. That can matter: a misspelled `default_profile` can fall
back to `base` and deactivate overlays only the intended profile selects, and a
misspelled `dependency_provider` skips Shdeps, including Dot's own upgrade. The
trade is deliberate: a hard error would also stop every host from pulling the
fix. `dot doctor` reports each ignored key as an `unknown configuration key
ignored` warning instead of printing to stderr. `dot init` lists ignored keys
in its plan and warns about keys that arrive with the cloned repository, after
its own report. `dot update` prints its warnings once it holds the update
lock, so a cron run that finds the lock busy stays quiet.

A misspelled key also degrades `dot update`, which otherwise converges in
full. When a pull brings one, that run warns as soon as it reloads the config,
because the rest of the run already uses the default. While the key is
present, every update that otherwise converges exits 1, ends with a warning
naming the key (unless quiet), and under `--cron` records `degraded update
config` in the update log instead of `ok`, so `dot doctor` reports `cron update
degraded: config failing` rather than a recent success. `dot init` warns but
does not fail over it.

A key with no suggestion that a pull brings stays silent for the rest of that
run, which exits 0: when the same run's Tools stage installs a Dot that knows
the key, nothing is printed. A key that is still unknown at the start of the
next command (no Dot release that knows it yet) warns there, including under
`dot update --cron`, where cron mails the warning until a release that knows it
arrives.

This protection exists only in Dot releases that include it. Older releases
still reject unknown keys with `dot: config: unknown key: <key>` and exit 2
from every operational command, including the update that would replace them.
Recover such a host by installing a newer Dot outside `dot update` (with the
Shdeps provider, `shdeps --force update`, which installs the Dot version the
client's Shdeps config selects), then run `dot update`. Profile definitions,
selectors, and overlay descriptors tolerate unknown keys too, with rules of
their own: see [Unknown keys in profile and overlay files](#unknown-keys-in-profile-and-overlay-files).

To introduce a config key:

1. Add it to Dot's parser, `KNOWN_KEYS`, and the table above, and ship that
   release. Choose a `[a-z_]+` name (every release rejects other spellings as
   malformed) more than two edits away from every existing key (a unit test
   enforces this), so older releases do not mistake it for a typo: they would
   warn about it mid-run and report every update as degraded until they
   upgrade.
2. Use the key in client repositories only after hosts can run that release.
   A host that has not upgraded yet warns and ignores the key, so its
   meaning must be safe to miss for one update cycle.
3. Never give an existing key a new value: older releases reject values they
   do not know. Add a new key instead.
4. Raise `version` only for a change older releases must not misread. That is
   the deliberate "requires a newer Dot" gate: an older Dot rejects the file
   with `unsupported version` from every operational command, so each lagging
   host needs the same out-of-band upgrade as above.

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

## Shdeps prune

Set `DOT_SHDEPS_PRUNE` in the environment of `dot update` to let it remove
orphaned Shdeps dependencies (ones no longer declared for the host) by running
`shdeps prune -y` itself:

- `never` (default, also when unset or empty): Dot never prunes; run
  `shdeps prune` yourself.
- `cron`: only `dot update --cron` prunes. `--quiet` and `DOT_QUIET` do not
  count; they change output, not what an update does.
- `always`: every `dot update` and `dot pull` prunes.

For example, a cron entry of `DOT_SHDEPS_PRUNE=cron dot update --cron` prunes
on every unattended run. `dot init` ignores the variable. Any other value
prints a warning and reads as `never`; it never fails the update. This is an
environment variable rather than a config key on purpose: the config file is
often shared through a client repository, and Dot releases from before
[unknown-key tolerance](#unknown-keys-and-version-skew) reject unknown config
keys, so a key would stop clients still running one of them. Older releases
simply ignore the variable and do not prune.

Pruning is destructive: Shdeps runs each orphan's `uninstall` hook and deletes
its managed payloads, links, and state. Dot reads the variable once and removes
it from the environment it passes to the dependency provider (including prune)
and to merge, lifecycle, and extension hooks. Plain helper processes such as
`git pull` still inherit Dot's own process environment.

Prune runs as its own `Prune` stage directly after `Tools`, while the update
still holds its lock. It uses the same prepared provider and Shdeps config
directory the `Tools` stage just converged, and runs only when that
generation's repository sync and overlay links succeeded, so it never acts on
a frozen or partially synchronized configuration. A failed `Tools` stage (for
example a dependency or post hook that keeps failing) does not stop it, and
merge hooks run after it either way. The stage is skipped, with a row naming
the reason, when sync or linking failed, profile deactivation failed, no
dependency provider is configured, or Shdeps is unavailable. An update that
exits early (a busy update lock, or a cron run skipped for unresolved local
edits) never reaches it. When a `Tools` run updates Dot itself, only the
continuation under the new revision prunes.

Because prune also runs after a failed `Tools` stage, renaming a dependency
can leave a gap: prune removes the old name's install as soon as it is no
longer declared, even if installing the replacement failed in the same run.
The replacement then appears on the next successful run, normally one cron
cycle later, but it stays missing for as long as its install keeps failing
(for example one that needs `sudo`, which a cron run cannot provide). Add the
replacement before removing the old entry when a gap matters.

Prune runs while the update holds its lock, so the lock is held for longer
than before. A manual `dot update` started during that window finds the lock
busy and exits 75 without doing anything, just as it would during any other
cron stage; rerun it once the cron run finishes.

Removal rows render under the stage; quiet and cron runs drop them and report
only Shdeps warnings on stderr plus one failure line. Output past 1 MiB per
stream is discarded rather than stopping the prune. Stdin is closed, but an
uninstall hook that needs `sudo` may still prompt on an interactive terminal.
Under cron there is no terminal: current Shdeps defers such an uninstall (and
a post hook that needs `sudo`) with one warning and exits 0, leaving it for a
later interactive run, while older Shdeps releases fail it. A prune failure
(for example a failed `uninstall` hook) marks the stage failed and makes the
update exit nonzero without stopping later stages or withholding the profile
lifecycle commit. A cron run whose only failures are `Tools` and/or `Prune`
(or a misspelled config key) is recorded as degraded rather than failed (see
[Cron update status](#cron-update-status)).

## Cron update status

Every `dot update --cron` run records its outcome under
`${XDG_STATE_HOME:-~/.local/state}/dot/` for `dot doctor`:

- `update.log` gets one line per run: `<epoch> <outcome> <stage>[ <detail>]`.
  The outcome is `ok`, `degraded`, `fail`, or `skip` (unresolved local edits;
  the detail lists them). A `degraded` line names the failing stages, for
  example `1790000000 degraded update tools,prune`. A run in which `Tools`
  updates Dot itself writes two lines, one per revision.
- `update.last-success` holds the epoch of the last fully clean run (exit 0).
- `update.last-converged` holds the epoch of the last run whose repository
  sync, overlay links, profile deactivation, and config hooks succeeded and
  whose profile lifecycle commit did not fail, followed by the failing stages
  when that run was degraded (for example `1790000000 prune`).

A run is degraded when everything except the `Tools` stage (a dependency, a
post hook, or an unavailable Shdeps) and/or the `Prune` stage succeeded: the
dotfiles are current, dependencies are not. This applies with or without
`DOT_SHDEPS_PRUNE`: a cron run whose only failure is `Tools` now logs
`degraded update tools` where it used to log `fail update`, so scripts that
search `update.log` for `fail` should also match `degraded`. A failed `Tools`
stage still defers the profile lifecycle commit to a later clean run, as
before. Any other failure is `fail` and refreshes neither stamp. A post hook
or uninstall that Shdeps defers because it needs `sudo` without a terminal
exits 0: that is expected, so the run stays `ok`, not degraded, and the
deferral shows up only as the Shdeps warning.

A run whose config holds a likely misspelled key (one the warning answers with
`did you mean`) is also degraded, with the `config` stage, for example
`1790000000 degraded update config`; it converged, but with that setting at its
default (see [Unknown keys and version skew](#unknown-keys-and-version-skew)).
An unknown key without a suggestion never degrades a run.

A degraded run still exits 1, like any other failed update. Cron mails
whatever a job prints (and a cron run prints only warnings and failures), not
its exit status, so the distinction lives in the recorded state instead of a
new exit code that existing callers would have to learn.

`dot doctor` checks both stamps against a two-hour window:

- a recent clean run: `cron update succeeded recently`;
- otherwise, a recent degraded convergence: `cron update degraded: <stages>
  failing`, with the time since the last clean run;
- otherwise: `cron update has not succeeded recently`, meaning the host has
  stopped converging (or has been asleep); this includes a host whose only
  recorded runs were degraded, which older releases reported as unknown;
- with no stamp at all: `cron update success is unknown`.

A Dot older than the convergence stamp ignores it and keeps its original
reading of `update.last-success`, so it reports a degraded host as not
succeeding. After upgrading, the degraded report appears once the first cron
run under the new Dot has recorded convergence.

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

Tracked root selectors may omit both `user` and `host` to define a global
default that overrides `default_profile`. Machine-local and personal selectors
must include at least one of those fields. Every supplied field must match.
User names come from `id -un` and compare exactly and case-sensitively.
Short hostnames come from `hostname -s`; both configured and current values are
ASCII-lowercased after removing one trailing dot. A selector containing both
`user` and `host` is more specific than a selector containing only one field,
and the most-specific matching level wins. A user-only or host-only record
overrides a global root selector, and a combined record overrides either. This
permits a fleet-wide compatibility default, a user-wide default, and per-host
exceptions. Multiple matches at the winning specificity may agree on a
profile; conflicting choices at that same specificity are a configuration
error. Less-specific disagreements are ignored.

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

## Unknown keys in profile and overlay files

Profile definitions, selectors, and overlay descriptors also reach a host
before its Dot is upgraded, so a well-formed key (`[a-z_]+=value`) that this
Dot does not know does not fail the run either. Unlike the config, these files
are read after the base pull (personal selectors after their overlay's pull),
so the run that pulls a change is the first to act on it, and what this Dot
does without the key depends on what missing it could cost:

| File | A key this Dot does not know |
| --- | --- |
| Profile definition | Ignored; the members it knows still apply |
| Selector (root, machine-local, or personal) | The selector never matches; if it could have chosen this host's profile, `base` is selected |
| Overlay descriptor that is selected, or any `sync=none` descriptor | The overlay is skipped: selected but never activated |

A definition only adds members, so missing a newer key can at worst select
fewer overlays. A selector is a predicate and a descriptor decides what is
cloned and linked: a key there is most likely one more condition, such as a
match restriction, a pinned revision, or a trust rule. Ignoring it could match
hosts, or clone and link an overlay, that the newer Dot would not, so both fail
closed.

A skipped selector whose `user` and `host` (if any) match this host, and that
is at least as specific as every matching selector, could have chosen this
host's profile. If it names a profile other than the one selection lands on
without it (the most specific match, or `default_profile` when nothing
matches), or if it is more specific than two matching selectors that tie,
selection falls back to `base`, the profile phase one already applies on every
host. Falling through instead could select more overlays than the newer Dot
would, because a skipped selector is often the one narrowing a shared host, or
revive a tie it settled. `base` is narrower in the usual layout, where every
profile includes it; a profile that does not include `base` can lose overlays
to the fallback and gain `base`'s, which phase one already clones everywhere.
A skipped selector that agrees, ranks below a match, or names another user or
host changes nothing, and one for another user or host is not reported. `dot
doctor` shows the fallback as `selected profile (base (skipped-selector))`.

A descriptor is validated in full before it is skipped, and one that its
`platforms` or `hosts` filter already excludes stays quiet, because the key
changes nothing on that host. A skipped descriptor still claims its overlay
name, so a later descriptor with the same name cannot take its place. Skipping
an overlay `base` selects also leaves its personal selectors unread, and the
newer Dot reads them (unlike those of an overlay that is merely unavailable),
so selection falls back to `base` as for a skipped selector that could have
won. Without `profiles.d`, `sync=git` descriptors keep their legacy handling:
unknown lines are ignored and the overlay activates.

Every rule these files already had still applies: `version=1` comes first and
is the only accepted version (definitions and selectors), plus key syntax,
value safety (definition and selector values never contain `|`, a tab, or a
carriage return), duplicate known keys, known values, and the rules that tie
keys together, such as a profile needing members and a selector needing
`profile`. A selector or descriptor holding a key this Dot does not know may
omit `user` and `host`, or `url` or `path`: the key may supply them in a newer
Dot, and this one skips the file either way. An unknown key that is within two
edits of a key the file knows still fails, with a suggestion:

```text
dot: profile: /home/user/.config/dot/profiles.d/dev.conf: unknown key: overlay (did you mean 'overlays'?)
  warning: invalid overlay descriptor /home/user/.config/dot/overlays.d/30-dev.conf: unknown key: platfroms (did you mean 'platforms'?)
```

Such a key is almost certainly a typo, and ignoring it could drop overlays
(running their deactivation hooks) or widen a selector. Because these files are
read after the base pull, failing never stops a host from pulling the fix: like
any profile or descriptor error, `dot update` exits 1 and skips its `Tools`
stage until the next run pulls a corrected file.

Every command that reads these files prints one warning per file and key that
can matter on this host: keys in the profiles this host includes, in selectors
that name its user or host (or neither), and in descriptors its `platforms`
and `hosts` filters do not exclude. When selection itself fails, keys in every
definition print, because there is no selected profile to narrow them to. That
covers `update`, `pull`, and `init` (once per run, though discovery repeats),
and `status`, `fetch`, `push`, `diff`, and `test`:

```text
dot: profile: warning: /home/user/.config/dot/profiles.d/dev.conf: unknown key 'future_key' ignored (newer dot?)
dot: profile: warning: /home/user/.config/dot/profile-selectors.d/10-host.conf: unknown key 'future_key'; selector skipped (newer dot?)
dot: profile: warning: /home/user/.config/dot/profile-selectors.d/00-default.conf: unknown key 'future_key'; selector skipped, profile 'base' selected (newer dot?)
dot: overlay: warning: /home/user/.config/dot/overlays.d/30-dev.conf: unknown key 'future_key'; overlay 'dev' skipped (newer dot?)
dot: profile: warning: /home/user/.config/dot/overlays.d/80-personal.conf: unknown key 'future_key'; overlay 'personal' selectors unread, profile 'base' selected (newer dot?)
```

Unlike a config key from a newer Dot, which stays quiet in the run that pulls
it, these warn in that run too, because it already acts on them. A skipped
selector or overlay can change the final overlay set: the links of an overlay
that was active are removed, its `profile-deactivate` entry point runs, and the
pre-sync reconcile stage no longer sees it. It returns on the first run of a
Dot that knows the key. Such a run still exits 0 and records `ok`, not
`degraded`, but unless every key is in a skipped selector that changed
nothing, it skips the `Prune` stage (`keys from a newer dot; prune skipped`):
the overlay set may lack dependency configs that come back once Dot upgrades,
and pruning in between would uninstall their packages. `dot doctor`
reports each key as a warning row instead (`unknown profile key ignored`,
`selector skipped: unknown key`, `personal selectors unread: overlay skipped`,
or `<overlay>: selected but skipped: unknown descriptor key`).

Dot releases built before this change reject any unknown key in these files.
On such a host `dot update` fails after the base pull, exits 1, and skips its
`Tools` stage, so it never upgrades itself; every later update, and `status`,
`fetch`, `push`, `diff`, `test`, and `doctor`, fails the same way until the key
is removed or Dot is upgraded outside `dot update`, as described above for the
config.

To introduce a key in one of these files:

1. Add it to Dot's parser and known-key list (`DEFINITION_KEYS`,
   `SELECTOR_KEYS`, or `DESCRIPTOR_KEYS`) and to the documentation, and ship
   that release. Choose a `[a-z_]+` name more than two edits away from every
   key that file knows or any earlier release knew (unit tests check the
   current list; keep a removed key in mind). Short names collide easily: `os`
   is two edits from `host`. Older releases reject a near miss as a typo.
2. Use the key only after hosts can run that release. Until then, a lagging
   host ignores it in a definition, skips the selector (possibly selecting
   `base`), or skips the overlay, for at least one update cycle.
3. Never give an existing key a new value or meaning: older releases reject
   values they do not know. A profile definition still needs a member older
   releases know, and a selector still needs `profile`.
4. Keep definition keys additive: missing one may select fewer overlays, never
   more. A definition key that removes or restricts members needs a `version`
   raise. Selector keys and keys in selected (or `sync=none`) descriptors may
   restrict, because older releases skip what they cannot read; legacy
   `sync=git` descriptors in a client without `profiles.d` ignore them
   instead. A selector key must not count toward specificity (older releases
   rank a skipped selector by `user` and `host` alone); changing how
   selectors rank needs a `version` raise.
5. Raise `version` in definitions or selectors only for a change older
   releases must not misread, as for the config. Descriptors have no version
   key: any new key already keeps older releases from activating the
   descriptor.
