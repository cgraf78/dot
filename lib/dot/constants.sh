# shellcheck shell=bash
# Private runtime constants derived from the active client and XDG roots.

# Private runtime modules use this internal helper name. Delegate to the public
# resolver so fallback and validation semantics have one implementation.
_dot_xdg_path() {
  dot_xdg_path "$@"
}

dot_xdg_path state dot/overlay-links || return
DOT_OVERLAY_MANIFEST=$REPLY
DOT_OVERLAY_LEGACY_MANIFEST=$HOME/.local/state/dot/overlay-links

# Re-exec through the checked-out runtime rather than PATH, where Graphviz or
# the client-owned launcher may own the same command name.
DOT_BIN=$DOT_SOURCE_ROOT/bin/dot

DOT_QUIET=${DOT_QUIET:-0}
DOT_VERBOSE=${DOT_VERBOSE:-0}

export DOT_OVERLAY_MANIFEST DOT_OVERLAY_LEGACY_MANIFEST
export DOT_BIN DOT_QUIET DOT_VERBOSE
