# shellcheck shell=bash
# Shared helpers for managing marked blocks in config files.
#
# A marked block looks like:
#   # <marker> begin
#   # DO NOT EDIT: changes will be overwritten by dot update
#   # source: /path/to/source
#   <content>
#   # <marker> end
#
# Functions:
#   _mb_build    — assemble a marked block string
#   _mb_strip    — remove a marked block from a string
#   _mb_strip_family — remove every marked block with a marker prefix
#   _mb_finalize — normalize and atomically publish stripped content + blocks
#   _mb_merge    — merge marked blocks into a file (hand-managed first)
#   _mb_merge_family — merge blocks after stripping a whole marker family

if ! declare -F _dot_sibling_tmp_for >/dev/null 2>&1; then
  # shellcheck source=temp.sh
  . "${BASH_SOURCE[0]%/*}/temp.sh"
fi

# Build a marked block string.
# Args: $1 = marker, $2 = source path, $3 = body content
# Returns the block via stdout.
_mb_build() {
  local marker="$1" source="$2" body="$3"
  # Strip vim/emacs modelines — useful in source files for editing,
  # but wrong inside a merged config file.
  body="$(printf '%s\n' "$body" | grep -v '^#[[:space:]]*vim:' | grep -v '^#[[:space:]]*-\*-')"
  body="${body#"${body%%[![:space:]]*}"}"
  body="${body%"${body##*[![:space:]]}"}"
  printf '%s\n%s\n%s\n%s\n%s' \
    "$marker begin" \
    "# DO NOT EDIT: changes will be overwritten by dot update" \
    "# source: $source" \
    "$body" \
    "$marker end"
}

# Strip a marked block from a string.
# Args: $1 = marker (matched as prefix of begin/end lines), $2 = input string
# Returns the result via stdout.
_mb_strip() {
  local marker="$1" input="$2"
  if [[ "$input" == *"$marker begin"* ]]; then
    # The marker is interpolated into a sed address, so escape basic-regex
    # metacharacters (and the `/` delimiter) in case a source name ever
    # contains them; otherwise the address could over-match or break sed.
    local esc
    esc=$(printf '%s' "$marker" | sed 's/[][\.*^$/]/\\&/g')
    printf '%s\n' "$input" | sed "/$esc begin/,/$esc end/d"
  else
    printf '%s\n' "$input"
  fi
}

# Strip every marked block whose marker starts with a family prefix.
#
# This is useful for merge layers that support many source fragments, such as
# SSH config fragments: when a source file moves from one fragment path to another,
# the old marker is no longer represented by any current block. The merge still
# needs to remove that stale generated block while preserving hand-managed
# entries outside the managed family.
_mb_strip_family() {
  local marker_prefix="$1" input="$2"
  awk -v marker_prefix="$marker_prefix" '
    index($0, marker_prefix) == 1 && $0 ~ / begin$/ {
      in_block = 1
      next
    }
    in_block {
      if (index($0, marker_prefix) == 1 && $0 ~ / end$/) {
        in_block = 0
      }
      next
    }
    { print }
  ' <<<"$input"
}

# Normalize unmanaged content, append managed blocks, and atomically publish
# the result. Callers own marker stripping because exact and family replacement
# deliberately have different semantics.
# Args: $1 = destination, $2 = stripped unmanaged content, $3..N = blocks
_mb_finalize() {
  local dst="$1" rest="$2"
  shift 2
  local -a blocks=("$@")

  # Squeeze repeated blank lines, then strip leading/trailing whitespace.
  # Prevents blank-line accumulation across repeated merges.
  rest="$(printf '%s\n' "$rest" | cat -s)"
  rest="${rest#"${rest%%[![:space:]]*}"}"
  rest="${rest%"${rest##*[![:space:]]}"}"

  # Build result: hand-managed entries first, then managed blocks.
  local result="$rest"
  local block
  for block in "${blocks[@]}"; do
    if [[ -n "$result" ]]; then
      result+=$'\n\n'"$block"
    else
      result="$block"
    fi
  done
  result+=$'\n'

  # Skip write if nothing changed.
  if [[ -f "$dst" ]] && printf '%s' "$result" | cmp -s - "$dst"; then
    return 0
  fi

  local tmp
  _dot_sibling_tmp_for "$dst" || return 1
  tmp="$REPLY"
  printf '%s' "$result" >"$tmp" || {
    rm -f "$tmp"
    return 1
  }
  chmod 600 "$tmp" || {
    rm -f "$tmp"
    return 1
  }
  _dot_publish_prepared_regular "$tmp" "$dst" || {
    rm -f "$tmp"
    return 1
  }
}

# Merge marked blocks into a file. Hand-managed content stays first
# (important for SSH first-match-wins), managed blocks append at the end.
#
# Args: $1 = destination file path
#       $2..N = marked block strings (each from _mb_build)
#
# Creates the file if it doesn't exist. Skips write if nothing changed.
# Sets permissions to 600 on the destination file.
_mb_merge() {
  local dst="$1"
  shift
  local -a blocks=("$@")

  # Ensure parent directory exists
  local dst_dir
  dst_dir="$(dirname "$dst")"
  if [[ ! -d "$dst_dir" ]]; then
    mkdir -p "$dst_dir"
    chmod 700 "$dst_dir"
  fi

  local current=""
  [[ -f "$dst" ]] && current="$(cat "$dst")"

  # Strip existing managed blocks from the current config.
  local rest="$current"
  local block marker
  for block in "${blocks[@]}"; do
    # Extract marker from the first line ("# marker begin" → "# marker")
    marker="${block%% begin*}"
    rest="$(_mb_strip "$marker" "$rest")"
  done

  _mb_finalize "$dst" "$rest" "${blocks[@]}"
}

# Merge marked blocks into a file after stripping a whole marker family.
# Hand-managed content stays first (important for SSH first-match-wins),
# managed blocks append at the end.
#
# Args: $1 = destination file path
#       $2 = marker prefix for the generated block family
#       $3..N = marked block strings (each from _mb_build)
#
# Creates the file if it doesn't exist. Skips write if nothing changed.
# Sets permissions to 600 on the destination file.
_mb_merge_family() {
  local dst="$1" marker_prefix="$2"
  shift 2
  local -a blocks=("$@")

  # Ensure parent directory exists
  local dst_dir
  dst_dir="$(dirname "$dst")"
  if [[ ! -d "$dst_dir" ]]; then
    mkdir -p "$dst_dir"
    chmod 700 "$dst_dir"
  fi

  local current=""
  [[ -f "$dst" ]] && current="$(cat "$dst")"

  # Strip the entire managed family in memory. This keeps source migrations
  # atomic: replacing one source fragment path with another removes the stale
  # block in the same final write that installs the replacement blocks.
  local rest="$current"
  rest="$(_mb_strip_family "$marker_prefix" "$rest")"

  _mb_finalize "$dst" "$rest" "${blocks[@]}"
}
