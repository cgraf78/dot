# Extensions

Extensions are disabled unless configuration declares both
`extension_api=1` and a normalized absolute `extensions_dir`.

Version 1 discovers only these collections beneath that directory:

- `pre-sync.d/`
- `merge-hooks.d/`
- `doctor.d/`
- `tests/`

Other directories—including client-owned `git-hooks/`, `sley-hooks/`, and
helper libraries—are ignored by the standalone engine.

Hook names use an optional numeric ordering prefix plus a lowercase identity.
`NAME.serial.sh` adds a barrier without changing the identity or lexical sort
key. Duplicate identities are fatal before any extension runs. Each extension
runs in a fresh invocation of the absolute Bash selected by the `dot` launcher;
`PATH` cannot substitute another interpreter. Workers use `--noprofile` and
`--norc`, start in `$HOME`, close stdin, set `umask 077`, enable
`errexit`/`nounset`/`pipefail`, clear every resettable trap, and start with
`extglob`, `nocasematch`, and `nullglob` disabled. POSIX shell semantics do not
let a shell reset signals it inherited as ignored, so those exact host
dispositions (notably GitHub runners' ignored `SIGPIPE`) can remain visible.
Each worker receives a different private `TMPDIR`.

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
- [`test-api-v1.tsv`](../lib/dot/test-api-v1.tsv)

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

Test extensions are executable `tests/*-test` programs. By default, `dot test`
runs those client suites in parallel. The provider-owned `dot` suite remains
available through `dot test dot`, or alongside client suites in an unfiltered
run when `DOT_TEST_INCLUDE_PROVIDER=1`; exact and prefix filters otherwise
select a subset. Each suite receives isolated temp, cache, and state roots,
closed stdin, `DOT_TEST_HOST_HOME`,
`DOT_TEST_SOURCE_HOME`, and an absolute `DOT_TEST_REPORTER`.
`DOT_TEST_TIMEOUT` names the same portable, versioned timeout command used by
the coordinator for suites that need bounded subcommands; its first argument
must be a finite positive duration in seconds, optionally suffixed with `s`,
`m`, `h`, or `d`. Set `DOT_TEST_TIMEOUT_EXPIRED_FILE` to a private path when a
caller must distinguish an enforced deadline from a child that itself exits
124; the command clears stale marker state before launch, removes the variable
from the child's environment, and creates the marker only for an actual
timeout. A suite must invoke the reporter exactly once with
`complete PASSED FAILED` or `skip [REASON]`. Exit zero without a valid record is
an incomplete failure, so display prose is never parsed as machine state. The
runner enforces a bounded timeout and terminates the suite process session on
timeout or cancellation.

The optional header `# dot-suite-priority: early` places a suite in the first
parallel worker wave without changing deterministic order within that wave.
Extensions execute as standalone programs with their declared interpreter and
normal user authority; they are trust-validated and isolated from one another,
not sandboxed.

The inventories remain machine-readable so CI can reject accidental additions,
removals, or signature drift. Extensions execute with the full authority of the
user; these checks prevent accidental substitution, not a same-user sandbox
escape.
