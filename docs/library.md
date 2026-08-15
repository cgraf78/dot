# Public shell library

The installed `~/.local/lib/dot` link exposes only `lib/dot/public`. Source
`api-version.sh` first and require `DOT_LIBRARY_API=1`.

## XDG API

```bash
dot_xdg_home config|state|cache|data
dot_xdg_path config|state|cache|data RELATIVE_SUFFIX
```

On success both functions set `REPLY`. XDG environment values are accepted
only when absolute; otherwise the matching HOME fallback is used. Unknown
kinds, invalid relative suffixes, and arity errors return 2; an unavailable
absolute HOME returns 1. `dot_xdg_path` accepts a nonempty normalized relative
suffix and rejects absolute paths and `.`/`..` navigation.

## UI API

```bash
dot_ui_color_hex NAME_OR_HEX
dot_ui_hex_to_rgb '#rrggbb'
dot_ui_has_gum
dot_ui_title TEXT...
dot_ui_summary_box COLOR TEXT...
```

`dot_ui_color_hex` maps a named color to `#rrggbb` and accepts an already
validated six-digit literal. `dot_ui_hex_to_rgb` converts that literal to the
semicolon-separated terminal form. Invalid color syntax and arity errors
return 2. `dot_ui_has_gum` returns 0 and sets `REPLY` to the validated
executable only when the discovered Gum can run its `style` subcommand, 1 when
Gum is unavailable, and 2 for API misuse.

The rendering helpers write human output to stdout and return the selected
renderer's status. `COLOR` may be `green`, `red`, `yellow`, `magenta`, `dim`,
or a six-digit literal hex color. Gum is optional and is used only after its
`style` subcommand passes a runtime probe.

[`api-v1.tsv`](../lib/dot/public/api-v1.tsv) is the machine-readable V1
function inventory. [`variables-v1.tsv`](../lib/dot/public/variables-v1.tsv)
does the same for the supported environment and result-variable surface.
Tests require both inventories, these docs, and the sourceable declarations to
stay in exact agreement; changing an existing contract needs a new API version
rather than an in-place drift.

## Environment and result variables

- `DOT_LIBRARY_API` is the exported integer `1` after `api-version.sh` loads.
- `HOME` supplies an absolute fallback root when an XDG base is empty or
  relative.
- `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, and `XDG_DATA_HOME`
  provide their corresponding absolute bases.
- `PATH` is consulted only for optional Gum discovery.
- A nonempty `NO_COLOR` disables ANSI styling in the plain terminal fallback.
- `REPLY` is cleared when an XDG or Gum-availability query starts, then carries
  a successful XDG path result or the validated Gum executable from
  `dot_ui_has_gum`; callers should consume it only after status 0. The title
  and summary renderers keep their Gum lookup private and preserve caller
  `REPLY`.

Everything outside `lib/dot/public` is private and may change without an API
version bump.
