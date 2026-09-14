# shellcheck shell=bash
# Shared support for dot merge hooks.
#
# Merge hooks interpret application-specific declarative policy. This file owns
# the repeated mechanics that must be consistent everywhere: locating tracked
# hook sources, expanding portable home placeholders, probing parser tools, and
# writing generated configs through a sibling temp file before replacing the
# destination.

if ! declare -F _dot_sibling_tmp_for >/dev/null 2>&1; then
  # shellcheck source=temp.sh
  . "${BASH_SOURCE[0]%/*}/temp.sh"
fi
if ! declare -F dot_family_files >/dev/null 2>&1; then
  # shellcheck source=families.sh
  . "${BASH_SOURCE[0]%/*}/families.sh"
fi

_merge_hook_dir() {
  dot_xdg_path config dot/merge-hooks.d || return
  printf '%s\n' "$REPLY"
}

_merge_hook_source() {
  printf '%s/%s\n' "$(_merge_hook_dir)" "$1"
}

# Expand the home-directory placeholders supported by declarative merge-hook
# config files.
#
# Keep this deliberately narrow: config files may use `$HOME`, `${HOME}`, or
# `~` to stay machine-portable, but the merge layer should not evaluate
# arbitrary shell syntax from user-editable policy files.
#
# Args: $1 = path or scalar value
# Returns the expanded value on stdout.
_merge_hook_expand_home() {
  local value="$1"

  value="${value//\$\{HOME\}/$HOME}"
  value="${value//\$HOME/$HOME}"

  case "$value" in
    '~') printf '%s\n' "$HOME" ;;
    \~/*) printf '%s/%s\n' "$HOME" "${value#\~/}" ;;
    *) printf '%s\n' "$value" ;;
  esac
}

# Resolve a merge-hook source family directory.
#
# Family names are opaque to dot core. The consumer chooses a directory name
# that makes sense for its target config; core only joins it to the shared
# merge-hooks source root so every hook and test uses the same location policy.
#
# Args: $1 = family directory name relative to merge-hooks.d
# Returns the absolute family directory path on stdout.
_merge_hook_family() {
  printf '%s/%s\n' "$(_merge_hook_dir)" "$1"
}

# Print the ordered source files for a merge-hook source family.
#
# This wrapper is intentionally thin: `dot_family_files` owns structural
# ordering and `.replace` mutual-exclusion, while merge hooks own parsing and
# target-specific merge behavior. Keeping callers on this wrapper makes later
# changes to family discovery one-library changes instead of hook-by-hook
# rewrites.
#
# Args: $1 = family directory name relative to merge-hooks.d
# Returns absolute source paths on stdout, one per line.
_merge_hook_family_files() {
  dot_family_files "$(_merge_hook_family "$1")"
}

# Print family files whose family-relative path matches one of the caller's
# shell patterns.
#
# Dot core's family policy intentionally does not understand target formats.
# A family can contain extensionless SSH fragments, JSON settings, TOML profile
# layers, or cron text. Consumers that own a native file format should filter at
# this boundary so a README or disabled non-matching file cannot accidentally
# become parser input.
#
# Args: $1 = family directory name relative to merge-hooks.d
#       $2..N = shell patterns matched against the family-relative path
# Returns matching absolute source paths on stdout, one per line.
_merge_hook_family_files_matching() {
  local family="$1"
  shift
  dot_family_files_matching "$(_merge_hook_family "$family")" "$@"
}

# Return a file's path relative to a merge-hook source family.
#
# Generated block markers need a stable name for each source, but basename-only
# markers collide once families support nested `.replace` groups. Relative
# family paths preserve ordering context without exposing absolute usernames or
# machine-specific paths in marker identities.
#
# Args: $1 = family directory name relative to merge-hooks.d
#       $2 = absolute file path inside that family
# Returns the relative path on stdout.
_merge_hook_family_relpath() {
  local family="$1" file="$2" dir
  dir="$(_merge_hook_family "$family")"
  printf '%s\n' "${file#"$dir/"}"
}

# Convert a family-relative file path to a marker-safe identifier.
#
# Marker strings live inside generated config comments, so keep them plain and
# deterministic. Slash replacement is enough for current family paths because
# file basenames cannot contain `/`; consumers still render the real path in
# the `source:` line for debugging.
#
# Args: $1 = family directory name relative to merge-hooks.d
#       $2 = absolute file path inside that family
# Returns a marker-safe identifier on stdout.
_merge_hook_family_marker_name() {
  local rel
  rel="$(_merge_hook_family_relpath "$1" "$2")"
  printf '%s\n' "${rel//\//_}"
}

_merge_hook_jq_available() {
  command -v jq >/dev/null 2>&1
}

_merge_hook_tmp_for() {
  local dst="$1"
  # Keep the temp file beside the destination so mv is atomic on the same
  # filesystem. The shared helper uses mktemp/O_EXCL so a same-user process
  # cannot pre-create a predictable symlink target before dot update writes.
  _dot_sibling_tmp_for "$dst"
}

_merge_hook_commit_tmp() {
  local tmp="$1" dst="$2"
  _dot_publish_prepared_regular "$tmp" "$dst"
}

_merge_hook_write_text_if_changed() {
  local dst="$1" text="$2" tmp

  if printf '%s\n' "$text" | _dot_stdin_matches_file "$dst"; then
    return 0
  fi

  _merge_hook_tmp_for "$dst" || return 1
  tmp="$REPLY"
  printf '%s\n' "$text" >"$tmp" || {
    rm -f "$tmp"
    return 1
  }
  _merge_hook_commit_tmp "$tmp" "$dst"
}

_merge_hook_jq_layer() {
  local label="$1" src="$2" dst="$3" filter="$4" tmp

  _merge_hook_tmp_for "$dst" || return 1
  tmp="$REPLY"

  if [[ ! -f "$dst" ]]; then
    if ! jq --sort-keys --indent 2 '.' "$src" >"$tmp"; then
      _warn "    warning: $label copy failed — skipping"
      rm -f "$tmp"
      return 0
    fi
    _merge_hook_commit_tmp "$tmp" "$dst"
    return 0
  fi

  # jq exits successfully for an empty file, so -s is part of the corruption
  # check. Rebuilding is safer than preserving unreadable generated config.
  if [[ ! -s "$dst" ]] || ! jq empty "$dst" 2>/dev/null; then
    _warn "    warning: corrupt $dst — rebuilding"
    rm -f "$dst" "$tmp"
    _merge_hook_jq_layer "$label" "$src" "$dst" "$filter"
    return 0
  fi

  if ! jq -n --sort-keys --indent 2 --slurpfile s "$src" --slurpfile d "$dst" \
    "$filter" >"$tmp"; then
    _warn "    warning: $label merge failed — skipping"
    rm -f "$tmp"
    return 0
  fi

  _merge_hook_commit_tmp "$tmp" "$dst"
}
