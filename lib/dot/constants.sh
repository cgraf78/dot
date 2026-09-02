# shellcheck shell=bash
# Private runtime constants derived from the active client and XDG roots.

dot_xdg_path state dot/overlay-links || return
DOT_OVERLAY_MANIFEST=$REPLY
DOT_OVERLAY_LEGACY_MANIFEST=$HOME/.local/state/dot/overlay-links
dot_xdg_path state dot/profile-overlay-lifecycle-v1 || return
DOT_PROFILE_LIFECYCLE_LEDGER=$REPLY

# Re-exec through the checked-out runtime rather than PATH, where Graphviz or
# the client-owned launcher may own the same command name.
DOT_BIN=$DOT_SOURCE_ROOT/bin/dot

DOT_QUIET=${DOT_QUIET:-0}
DOT_VERBOSE=${DOT_VERBOSE:-0}

export DOT_OVERLAY_MANIFEST DOT_OVERLAY_LEGACY_MANIFEST
export DOT_PROFILE_LIFECYCLE_LEDGER
export DOT_BIN DOT_QUIET DOT_VERBOSE
