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
fix. `dot doctor` reports each ignored key as `unknown configuration key
ignored` instead of printing to stderr: a failure for a likely misspelling
(every `dot update` exits 1 while it is present, see below) and a warning for
any other key. `dot init` lists ignored keys
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

`dot update --cron` removes orphaned Shdeps dependencies (ones no longer
declared for the host) by running `shdeps prune -y` itself. Set
`DOT_SHDEPS_PRUNE` in the environment of `dot update` to choose when:

- `cron` (default, also when unset or empty): only `dot update --cron` prunes.
  `--quiet` and `DOT_QUIET` do not count; they change output, not what an
  update does.
- `never`: Dot never prunes, including under `--cron`; run `shdeps prune`
  yourself.
- `always`: every `dot update` and `dot pull` prunes.

For example, a plain cron entry of `dot update --cron` prunes on every
unattended run. `dot init` ignores the variable. Any other value prints a
warning and reads as `never`, so a misspelled value also turns off the default
cron prune; it never fails the update. This is an environment variable rather
than a config key on purpose: the config file is often shared through a client
repository, and Dot releases from before
[unknown-key tolerance](#unknown-keys-and-version-skew) reject unknown config
keys, so a key would stop clients still running one of them.

Older releases defaulted to `never` (also for an empty value), so cron prune
was opt-in through `DOT_SHDEPS_PRUNE=cron dot update --cron`. That entry keeps
working unchanged. A plain `dot update --cron` entry does not prune on a host
still running an older release: when its `Tools` stage upgrades Dot and hands
the run to the new release, the continuation already prunes; an older release
that finishes the run in place prunes from the next cron run on. Releases from
before `DOT_SHDEPS_PRUNE` existed ignore the variable and never prune. The
first cron run of a release with the new default on a host that never pruned
removes every orphan accumulated so far; set `DOT_SHDEPS_PRUNE=never` in the
cron entry beforehand to keep them.

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

Every update run, with or without `--cron` (including `dot init`
convergence), also overwrites `update.last-run` with
`<epoch> <outcome> <trigger>[ <stages>]`: the same `ok`, `degraded`, or `fail`
classification (or `skip` for a cron run skipped for local edits), the
trigger `cron`, `manual`, or `init`, and the failing stages of a degraded run
(for example `1790000000 degraded manual tools`). Interrupted runs leave it
unchanged.

Every run that does not succeed (any trigger, including a cron skip) also
overwrites `update.last-failure` with its cause. The first line repeats the
run's `<epoch> <outcome> <trigger>` from `update.last-run`; each further line
is `item<TAB><stage><TAB><name><TAB><detail>`: a failed Shdeps item and its
detail, a merge-hook key and the last line of its output, a repository that
failed to pull, a file whose local edits skipped a cron run, or the stage's
own reason (for example Shdeps' last stderr line when it stopped before
reporting any item). At most five items per stage are kept, followed by
`more<TAB><stage><TAB><count>`; fields have URL credentials redacted, are
stripped of control characters, and are capped, and the file stays within
4 KiB. Because an item can carry a line of
hook or Shdeps output, the file is owner-only like the rest of this directory
and a clean run removes it. Doctor shows a cause only while its header matches
`update.last-run`, so a cause left by an older run (or next to a stamp written
by a Dot that does not write this file) never explains a newer run. Beyond
`update.last-run` and `update.last-failure`, hand-run updates write nothing.

A run is degraded when everything except the `Tools` stage (a dependency, a
post hook, or an unavailable Shdeps) and/or the `Prune` stage succeeded: the
dotfiles are current, dependencies are not. This applies whether or not the
run prunes: a cron run whose only failure is `Tools` now logs
`degraded update tools` where it used to log `fail update`, so scripts that
search `update.log` for `fail` should also match `degraded`. A failed `Tools`
stage still defers the profile lifecycle commit to a later clean run, as
before. Any other failure is `fail` and refreshes neither stamp. A post hook
or uninstall that Shdeps defers because it needs `sudo` without a terminal
exits 0: that is expected, so the run stays `ok`, not degraded, and the
deferral shows up only as the Shdeps warning.

A failed base pull is `fail` in every mode, whatever the cause (an
unreachable dotfiles remote, a conflicted or interrupted rebase, a checkout
that fails validation). It fails a quiet or cron run exactly as it fails a hand
run: the update exits 1, keeps the installed overlay links, and skips overlay
pulls, profile deactivation, `Tools` (including Dot's own upgrade), `Prune`,
and config hooks until the base pulls again. Quiet only hides the rows;
`warning: dotfiles pull failed` on stderr reports the failure instead. Older
releases left a quiet base-pull failure out of the tally, so a cron run against
an unreachable remote exited 0, logged `ok`, refreshed both stamps, and wrote
`ok` to `update.last-run`.

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

- a recent cron run skipped for local edits after the last clean run: `cron
  update skipping: local edits block it`, naming the edited files and
  pointing at `dot status`, whatever the age of that clean run (cron stays
  frozen until the edits are resolved); a skip older than the window means
  cron stopped too and reads as below;
- a recent clean run: `cron update succeeded recently`, unless a cron run
  after it did not succeed: then `last cron run failed` (or `degraded:
  <stages> failing`), because a recent clean run no longer hides a newer
  failure. A single failed run (a network blip) therefore warns until the
  next clean run;
- otherwise, a recent degraded convergence: `cron update degraded: <stages>
  failing`, with the time since the last clean run, unless a newer cron run
  did not converge at all, which reads `last cron run failed`;
- otherwise: `cron update has not succeeded recently`, meaning the host has
  stopped converging (or has been asleep); this includes a host whose only
  recorded runs were degraded, which older releases reported as unknown. It
  names the last cron run and its cause, or, when no failing cron run was
  recorded, suggests checking that `dot update --cron` is still scheduled;
- with no cron stamp, `update.last-run` decides: a cron last run that failed
  reads `cron update has not succeeded recently`; a hand-run last
  update older than the window reads `cron update has never run`, a warning
  (a scheduled `dot update --cron` would have run by then) or only a skip
  when no `crontab` is on `PATH` (Termux, containers); a newer one reads
  `cron update has not run yet`;
- with no stamp at all: `cron update success is unknown`.

Every row about a run that did not succeed lists its failing items from
`update.last-failure` when that record describes the run, one per line, for
example `- tools: watchexec/watchexec (ambiguous interrupted method
transition)` (up to five, then `+N more`, which also counts items the record
did not keep; a leading `error: ` is dropped and a long cause is shortened
after a whole word). The next step follows on its own `→` line, like every
other warning's: `shdeps health` when a dependency or prune failed,
otherwise `dot update` for the full output (including when Shdeps itself
could not be prepared). Records written by older Dots have the same items and
render the same way. State written by a Dot
older than the record shows the same rows without the cause. URL credentials
(`https://user:token@host`) are redacted to `https://***@host` both when the
record is written and when it is shown, so a record an older Dot wrote is
redacted on display too (unless its own length cap cut the URL before the
`@`).

`dot doctor` also reports the last hand-run (`manual` or `init`) update when
it adds information: `last update succeeded`, `last update degraded: <stages>
failing`, or `last update failed`, always without a recent clean cron run and
otherwise only when it did not succeed after that run. A failed hand-run
update warns: conditions that make every update exit 1 have rows of their own,
and a one-off failure heals on the next run.

When `Tools` updates Dot twice during one update, the run exits 1 and leaves
`provider-reexec-failed` in the same directory for the next update to validate
against the active Dot. `dot doctor` warns while that record is pending and
fails when the next update cannot consume it (an unsafe or malformed record,
or one pinning a revision other than the active Dot), because every update
then exits 1 until it is inspected and removed.

A Dot older than the convergence stamp ignores it and keeps its original
reading of `update.last-success`, so it reports a degraded host as not
succeeding. After upgrading, the degraded report appears once the first cron
run under the new Dot has recorded convergence. Likewise a Dot older than
`update.last-run` neither writes nor reads it: until an update under a newer
Dot records one, a hand-updated host still reads unknown.

### Doctor severities

`dot doctor` fails on every condition that makes `dot update` refuse or exit
1, and warns on conditions that update survives or that clear themselves:

- a likely misspelled configuration key fails; a key from a newer Dot warns;
- an update lock whose owner cannot be verified fails (mutating commands
  refuse until the probe succeeds); a live owner, a stale owner (reclaimed by
  the next command), a lock being initialized, and a probe interrupted by a
  signal to doctor warn. A live owner's row says how long it has held the lock
  (from its owner record's mtime), and one held for more than two hours reads
  `update has been running for <age>`, most likely hung, with its pid to stop;
- an overlay whose origin differs from its descriptor fails (update refuses
  to pull or link it), with the command that adopts the configured URL;
- a provider re-exec checkpoint the next update cannot consume fails;
- unmerged paths in the client or a Git overlay fail when update would
  refuse to pull over them (on a branch with an upstream, outside a merge or
  rebase) and warn while a merge or rebase the user started is in progress;
  tracked changes, a detached HEAD, a missing upstream, and upstream
  distance warn;
- a checkout whose HEAD is on no branch is classified the way `dot update`
  classifies it: dot's own interrupted rebase fails while the checkout has
  uncommitted changes (every update fails until it is aborted, optional
  overlays included) and warns otherwise (the next update aborts it and
  pulls); a merge or rebase the user started and a plain detached HEAD warn
  (update skips the checkout);
- a rebase `dot update` froze after it conflicted (the same HEAD, on a
  branch with an upstream, is recorded in the repository's
  `dot-rebase-failed` marker) fails until it is rebased by hand; for an
  optional overlay it warns.

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
Short hostnames are the kernel host name cut at its first dot (what
`hostname -s` prints); both configured and current values are
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
| Selector (root, machine-local, or personal) | The selector never matches; if it could have chosen this host's profile, `dot update` holds the installed overlay set |
| Overlay descriptor that is selected, or any `sync=none` descriptor | The overlay is skipped (never activated), and `dot update` holds the installed overlay set |

A definition only adds members, so missing a newer key can at worst select
fewer overlays. A selector is a predicate and a descriptor decides what is
cloned and linked: a key there is most likely one more condition, such as a
match restriction, a pinned revision, or a trust rule. Ignoring it could match
hosts, or clone and link an overlay, that the newer Dot would not, so both fail
closed. Converging to that partial reading would not be safe either: it would
deactivate overlays the newer Dot keeps. So `dot update` holds the overlay set
instead (see [Held overlay sets](#held-overlay-sets)).

A skipped selector whose `user` and `host` (if any) match this host, and that
is at least as specific as every matching selector, could have chosen this
host's profile. If it names a profile other than the one selection lands on
without it (the most specific match, or `default_profile` when nothing
matches), or if it is more specific than two matching selectors that tie,
this Dot cannot tell which profile applies. Falling through could select more
overlays than the newer Dot would, because a skipped selector is often the one
narrowing a shared host, or revive a tie it settled; `dot update` holds the
overlay set instead. Commands that only report a selection (`status`, `fetch`,
`push`, `diff`, `test`, and `doctor`) show `base`, the profile phase one
already applies on every host, as `profile base (skipped-selector; ...)`.
A skipped selector that agrees, ranks below a match, or names another user or
host changes nothing, and one for another user or host is not reported.

A descriptor is validated in full before it is skipped, and one that its
`platforms` or `hosts` filter already excludes stays quiet, because the key
changes nothing on that host. A skipped descriptor still claims its overlay
name, so a later descriptor with the same name cannot take its place. Skipping
an overlay `base` selects also leaves its personal selectors unread, and the
newer Dot reads them (unlike those of an overlay that is merely unavailable),
so the selection is unknown as well: `dot update` holds before it pulls
anything, and reporting commands show `base`. Without `profiles.d`, `sync=git`
descriptors keep their legacy handling: unknown lines are ignored and the
overlay activates.

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
dot: overlay: warning: overlay set held: newer keys need a newer dot (installed overlays, links, and config hooks left as they are)
```

Unlike a config key from a newer Dot, which stays quiet in the run that pulls
it, these warn in that run too, because it already acts on them. `dot doctor`
reports each key as a warning row instead (`unknown profile key ignored`,
`selector skipped: unknown key`, `personal selectors unread: overlay skipped`,
or `<overlay>: selected but skipped: unknown descriptor key`). A run with only
ignored definition keys, or skipped selectors that changed nothing, converges
normally but skips the `Prune` stage (`keys from a newer dot; prune skipped`)
unless every key is such a selector: the overlay set may lack dependency
configs that a newer Dot would link.

### Held overlay sets

When a key makes the overlay set uncertain (a skipped descriptor, a skipped
selector that could have chosen this host's profile, or a skipped `base`
overlay whose personal selectors went unread), `dot update`, `pull`, and
`init` hold the installed overlay set rather than converge to this Dot's
partial reading, which could unlink every overlay but `base`'s and run their
`profile-deactivate` entry points. A held run:

- leaves every installed overlay link untouched and activates nothing. When
  a selector or a non-`base` descriptor triggers the hold, phase one has
  already run the pre-sync `prepare` stage and refreshed (or, on a fresh host,
  cloned) the `base` overlays, so their new content shows through the
  existing links; a skipped `base` overlay holds before any of that. No other
  overlay is pulled;
- runs no `profile-deactivate` entry point, pre-sync `reconcile` stage, or
  config hook, and commits no lifecycle change;
- still runs `Tools`, so the host can install the Dot that knows the keys,
  but skips `Prune`;
- prints `dot: overlay: warning: overlay set held: newer keys need a newer
  dot (...)` once, exits 0, and records `ok` under `--cron`.

A fresh host with nothing installed stays empty: nothing is activated from a
selection this Dot cannot read. The next run of a Dot that knows the keys (or
any run after the keys are removed) converges normally. Until then every run
holds, so overlay and config-hook changes wait. `dot doctor` reports `overlay
set held: newer keys need a newer dot` and does not check overlay link
ownership against its partial reading.

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
   current list; keep a removed key in mind). Short names collide easily: a
   selector can never gain `hosts` (one edit from `host`) or `os` (two edits
   from `host`), because older releases reject a near miss as a typo, and that
   fails `dot update` before Tools.
2. Use the key only after hosts can run that release. Until then, a lagging
   host ignores it in a definition, or holds its overlay set when a selector
   or descriptor needs it, for at least one update cycle.
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
