# shellcheck shell=bash
# Generic ordered fragment-family discovery.
#
# A family is an arbitrary directory whose files form one ordered input stream
# for a consumer. Dot core owns only the structural policy:
#
#   - direct files aggregate
#   - an immediate <name>.replace/ directory contributes only its last lexical
#     file, making that directory a mutually-exclusive group
#   - the final stream is sorted by relative path
#
# Consumers own file format, validation, merge semantics, and output targets.

# Return whether a path is a real fragment file.
#
# Family discovery is intentionally conservative about editor artifacts and
# dotfiles because these directories are shared by humans, overlays, and
# generated tooling. A stray swap file should never become config input during
# `dot update`, and hidden files are reserved for tooling metadata rather than
# user-authored merge fragments.
#
# Args: $1 = candidate path
# Returns 0 when the path is a consumable file, 1 otherwise.
_dot_family_is_file_candidate() {
  local path="$1" base
  [[ -f "$path" ]] || return 1
  # Every candidate comes from a non-trailing glob path. Shell suffix removal
  # is therefore equivalent to basename here, without starting a process for
  # every fragment considered during each convergence run.
  base="${path##*/}"
  case "$base" in
    .* | *~ | *.tmp | *.tmp.* | *.bak | *.swp | *.swo | *.DS_Store)
      return 1
      ;;
    *)
      return 0
      ;;
  esac
}

# Return whether a family-relative key matches the caller's optional patterns.
#
# Pattern filtering lives in dot core, not in each hook, because `.replace`
# groups must select the last matching candidate. Filtering after winner
# selection would let a README or disabled non-native file suppress a valid
# lower-priority fragment in the same group.
#
# Args: $1 = family-relative key
#       $2..N = optional shell patterns
# Returns 0 when no patterns were supplied or any pattern matches.
_dot_family_key_matches() {
  local key="$1" pattern
  shift
  (($# == 0)) && return 0

  for pattern in "$@"; do
    # shellcheck disable=SC2254 # Family filters are caller-owned glob patterns.
    case "$key" in
      $pattern) return 0 ;;
    esac
  done
  return 1
}

# Emit relative stream keys for direct aggregate files in a family.
#
# Direct children of the family directory are always aggregate layers. The
# helper emits only the relative key, not an absolute path, so the final sort
# can use the same namespace for aggregate files and selected `.replace`
# winners without relying on fragile delimiter records.
#
# Args: $1 = family directory
#       $2..N = optional shell patterns matched against relative keys
# Returns relative keys on stdout, one per line.
_dot_family_emit_direct_keys() {
  local dir="$1" file base
  shift

  for file in "$dir"/*; do
    _dot_family_is_file_candidate "$file" || continue
    base="${file##*/}"
    _dot_family_key_matches "$base" "$@" || continue
    printf '%s\n' "$base"
  done
}

# Emit relative stream keys for selected `.replace` group winners.
#
# An immediate `<name>.replace/` directory models mutual exclusivity. All files
# inside that one group compete with each other, and only the last lexical file
# wins. The selected key remains nested under the group path, so normal numeric
# prefixes on the group directory position the winner among aggregate files.
#
# The traversal is deliberately one level deep: deeper recursion would make
# the structural policy depend on arbitrary app-specific organization, which
# belongs in the consumer. If a consumer needs nested native config, that
# nesting should live inside the fragment file itself.
#
# Args: $1 = family directory
#       $2..N = optional shell patterns matched against relative keys
# Returns relative keys on stdout, one selected file per non-empty group.
_dot_family_emit_replace_keys() {
  local dir="$1" group group_base file file_base key selected_base
  shift

  for group in "$dir"/*.replace; do
    [[ -d "$group" ]] || continue
    group_base="${group##*/}"
    selected_base=""

    # The group winner is selected before the final stream sort. This keeps the
    # policy independent from the consumer's merge algorithm while still letting
    # ordinary numeric filename prefixes express priority inside the group.
    while IFS= read -r file; do
      [[ -n "$file" ]] || continue
      file_base="${file##*/}"
      key="$group_base/$file_base"
      _dot_family_key_matches "$key" "$@" || continue
      selected_base="$file_base"
    done < <(
      for file in "$group"/*; do
        _dot_family_is_file_candidate "$file" || continue
        printf '%s\n' "$file"
      done | LC_ALL=C sort
    )

    [[ -n "$selected_base" ]] || continue
    printf '%s/%s\n' "$group_base" "$selected_base"
  done
}

# Print the ordered source stream for a fragment family.
#
# This is the shared policy boundary for dotfiles fragment collections. It
# intentionally returns concrete file paths and leaves parsing/validation to
# the hook or component that owns the target format. Keeping that split lets
# SSH, cron, VS Code, Codex, and future consumers share ordering and
# mutual-exclusion semantics without turning dot core into an app-specific
# merge engine.
#
# Missing family directories are a no-op so overlays can add families
# incrementally. Empty `.replace` groups, or groups containing only ignored
# files, are also no-ops; an overlay can therefore carry disabled fragments
# without changing generated config.
#
# Args: $1 = family directory
# Returns absolute source paths on stdout, one per line; status 2 for invalid
# arguments.
dot_family_files() {
  [[ $# -eq 1 ]] || return 2
  dot_family_files_matching "$1"
}

# Print the ordered source stream for a typed fragment family.
#
# This is the same structural policy as dot_family_files, with caller-supplied
# shell patterns applied before `.replace` winners are selected. It is useful
# for consumers whose family directory may contain documentation or disabled
# files that are not native config fragments for that target.
#
# Args: $1 = family directory
#       $2..N = shell patterns matched against relative keys
# Returns absolute source paths on stdout, one per line; status 2 when the
# required directory argument is missing.
dot_family_files_matching() {
  [[ $# -ge 1 ]] || return 2
  local dir="$1"
  shift
  [[ -d "$dir" ]] || return 0

  {
    _dot_family_emit_direct_keys "$dir" "$@"
    _dot_family_emit_replace_keys "$dir" "$@"
  } | LC_ALL=C sort -u | while IFS= read -r key; do
    [[ -n "$key" ]] || continue
    printf '%s/%s\n' "$dir" "$key"
  done
}
