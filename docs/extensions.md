# Extensions

Extensions are disabled unless configuration declares both
`extension_api=1` and a normalized absolute `extensions_dir`.

Version 1 discovers only these collections beneath that directory:

- `pre-sync.d/`
- `merge-hooks.d/`
- `doctor.d/`

Other directories—including client-owned `git-hooks/`, `sley-hooks/`, helper
libraries, and tests—are ignored by the standalone engine.

Hook names use an optional numeric ordering prefix plus a lowercase identity.
`NAME.serial.sh` adds a barrier without changing the identity or lexical sort
key. Duplicate identities are fatal before any extension runs. Each extension
runs in a fresh invocation of the absolute Bash selected by the `dot` launcher;
`PATH` cannot substitute another interpreter. Workers use `--noprofile` and
`--norc`, start in `$HOME`, close stdin, set `umask 077`, enable
`errexit`/`nounset`/`pipefail`, clear traps, and start with `extglob`,
`nocasematch`, and `nullglob` disabled. Each worker receives a different
private `TMPDIR`.

Ordinary exported client variables remain available because configuration
policy can legitimately depend on platform and tool context. Bash startup and
parser controls do not: exported functions, `BASH_ENV`, `ENV`, `CDPATH`,
`GLOBIGNORE`, `BASH_COMPAT`, `POSIXLY_CORRECT`, `BASH_XTRACEFD`, `BASHOPTS`,
and `SHELLOPTS` are removed before the fresh interpreter starts. XDG variables
are replaced with the absolute roots already resolved by `dot`. A worker can
change its own functions, variables, options, or traps, but that state cannot
reach the coordinator or another extension. Merge hooks define `merge()`;
pre-sync extensions define `prepare()`; doctor extensions define `doctor()`.
All entry points receive no arguments.

Pre-sync extensions run serially in lexical order after candidate and local-
overlay preflight but before any repository fetch, pull, clone, or checkout
mutation. The first failure aborts repository synchronization. They use the
same hook API and fresh-worker trust boundary as merge hooks, but exist only
for client-owned prerequisites that genuinely must precede network access (for
example an SSH host-alias block needed to clone an optional private overlay).
Generic dot does not interpret the prepared application or transport state.

The configured extension root and every directory component beneath it must be
real directories, user-owned, and not group/other writable. Implementations
must also be user-owned, not group/other writable, and have one hard link. A
leaf implementation symlink is accepted only when the active private overlay
manifest authorizes its exact literal target, the recorded owner is a currently
active Git overlay with the expected origin, and the resolved file remains
inside that owned checkout's `home/` tree. Stale manifest entries and symlinked
directory components are rejected. Support modules are loaded with
`dot_hook_source RELATIVE_PATH`, which applies the same validation immediately
before sourcing. They share the worker's global scope and see no positional
arguments; use ordinary assignments or functions at top level, not `local`,
which is meaningful only inside a function body.

## API contract

The supported helper inventories are the normative signature/status/result
reference:

- [`hook-api-v1.tsv`](../lib/dot/hook-api-v1.tsv)
- [`doctor-api-v1.tsv`](../lib/dot/doctor-api-v1.tsv)

Arguments are positional and must match the inventory exactly; fixed-arity
helpers return 2 for misuse. Values printed by a helper go to stdout. Allocation
helpers instead set the worker-global `REPLY`, which callers must consume before
calling another helper. File helpers run under the top-level `dot update` lock,
but parallel hooks must not target the same destination; name a hook
`NAME.serial.sh` when it shares mutable state with another hook. A temporary
returned by `dot_sibling_tmp_for` belongs to the caller until
`dot_commit_tmp TEMP DESTINATION` consumes that prepared sibling or the caller
removes it after failure.

This merge hook exercises every hook API surface. Real hooks normally use only
the subset owned by their target format:

```bash
merge() {
  dot_hook_source merge-hooks.d/lib/parser.sh || return
  family=$(dot_hook_family example) || return
  dot_hook_family_files example >/dev/null || return
  dot_hook_family_files_matching example '*.json' >/dev/null || return
  while IFS= read -r source; do
    relative=$(dot_hook_family_relpath example "$source") || return
    marker=$(dot_hook_family_marker_name example "$source") || return
    dot_hook_log "reading $relative"
  done < <(dot_family_files "$family")
  dot_family_files_matching "$family" '*.json' >/dev/null || return

  dot_hook_platform_match 'linux,macos' || return 0
  dot_hook_host_match '!retired-host' || return 0

  expanded=$(dot_expand_home '$HOME/.config/example/config') || return
  dot_xdg_path config example/config || return
  destination=$REPLY
  dot_hook_log "HOME-relative input expands to $expanded"

  dot_sibling_tmp_for "$destination" || return
  temporary=$REPLY
  printf '%s\n' generated >"$temporary" || return
  dot_commit_tmp "$temporary" "$destination" || return
  dot_write_text_if_changed "$destination" generated || return

  if dot_json_available; then
    dot_json_layer example layer.json settings.json '.d[0] * .s[0]' || return
  fi
  block=$(dot_managed_block_build '# dot:example' source.conf body) || return
  dot_managed_block_strip '# dot:example' "$block" >/dev/null || return
  dot_managed_block_strip_family '# dot:' "$block" >/dev/null || return
  dot_managed_block_merge "$HOME/.config/example/managed.conf" "$block" || return
  dot_managed_block_merge_family \
    "$HOME/.config/example/family.conf" '# dot:' "$block" || return
  dot_tool_present git || dot_hook_warn 'git is unavailable'
}
```

`dot_json_layer LABEL SOURCE DESTINATION FILTER` treats parser/merge rejection as
a handled warning and leaves or rebuilds the destination according to the
documented generated-config recovery policy; allocation or publication failure
returns nonzero. `dot_write_text_if_changed` preserves an unchanged destination
inode. Family helpers print one deterministic path per line, with filtering
applied before `.replace` winner selection.

Doctor extensions report structured records only; ordinary stdout/stderr is
diagnosed as out-of-band output. Each result helper accepts `LABEL [DETAIL]`
except the one-argument section helper:

```bash
doctor() {
  dot_doctor_source doctor.d/lib/checks.sh || return
  dot_doctor_section 'Example'
  dot_doctor_ok 'configuration exists' "$(dot_doctor_display_path "$HOME/.config/example")"
  dot_doctor_warn 'optional cache missing' 'it will be rebuilt'
  dot_doctor_fail 'required executable missing' 'install example-tool'
  dot_doctor_skip 'remote probe' 'offline mode'
}
```

The coordinator owns rendering, counters, ordering, and aggregate exit status;
extensions must not inspect or mutate those internals. `dot_doctor_display_path`
only formats a path for human output and has no filesystem side effects.

The inventories remain machine-readable so CI can reject accidental additions,
removals, or signature drift. Extensions execute with the full authority of the
user; these checks prevent accidental substitution, not a same-user sandbox
escape.
