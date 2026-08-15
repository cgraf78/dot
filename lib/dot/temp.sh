# shellcheck shell=bash
# Shared helpers for safe sibling temp files.

_dot_sibling_tmp_for() {
  local dst="$1" dir base tmp
  dir="$(dirname "$dst")"
  base="$(basename "$dst")"

  mkdir -p "$dir" || return 1
  tmp=$(mktemp "$dir/${base}.tmp.XXXXXX" 2>/dev/null) || return 1
  REPLY="$tmp"
}

_dot_path_identity() {
  stat -c '%d:%i' "$1" 2>/dev/null || stat -f '%d:%i' "$1" 2>/dev/null
}

_dot_detect_move_tool() {
  local probe source moved mv_bin
  mv_bin=$(type -P mv 2>/dev/null) || return 1
  if [[ -n ${DOT_MOVE_BIN:-} && -n ${DOT_MOVE_MODE:-} &&
    -x ${DOT_MOVE_BIN:-} && $DOT_MOVE_BIN == "$mv_bin" ]]; then
    return 0
  fi
  unset DOT_MOVE_BIN DOT_MOVE_MODE
  probe=$(mktemp -d "${TMPDIR:-/tmp}/dot-move-tools.XXXXXX") || return 1
  source=$probe/source
  moved=$probe/moved
  mkdir "$source"
  if "$mv_bin" -nT -- "$source" "$moved" 2>/dev/null &&
    [[ -d $moved && ! -e $source ]]; then
    DOT_MOVE_MODE=T
  else
    rm -rf "$source" "$moved"
    mkdir "$source"
    if "$mv_bin" -nh "$source" "$moved" 2>/dev/null &&
      [[ -d $moved && ! -e $source ]]; then
      DOT_MOVE_MODE=h
    else
      rm -rf "$source" "$moved" "$probe"
      return 1
    fi
  fi
  rm -rf "$source" "$moved"
  rmdir "$probe" 2>/dev/null || true
  DOT_MOVE_BIN=$mv_bin
}

# Move one prepared sibling into an absent destination without replacing a
# late file, symlink, or empty directory. BSD mv can briefly nest the source in
# a late directory; exact inode recovery moves only that source back out.
_dot_move_noreplace() {
  local source=$1 target=$2 identity target_identity nested
  _dot_detect_move_tool || return 1
  identity=$(_dot_path_identity "$source") || return 1
  if [[ $DOT_MOVE_MODE == T ]]; then
    "$DOT_MOVE_BIN" -nT -- "$source" "$target" 2>/dev/null || true
  else
    "$DOT_MOVE_BIN" -nh "$source" "$target" 2>/dev/null || true
  fi
  target_identity=$(_dot_path_identity "$target" 2>/dev/null || true)
  [[ $target_identity == "$identity" ]] && return 0
  nested=$target/${source##*/}
  if [[ -d $target && ! -L $target &&
    $(_dot_path_identity "$nested" 2>/dev/null || true) == "$identity" ]]; then
    mv "$nested" "$source" 2>/dev/null || return 1
  fi
  return 1
}

# Replace a known engine-owned non-directory destination without ever leaving
# the prepared source nested in a late directory or a symlink-to-directory.
# Callers must validate any existing destination before invoking this helper.
_dot_move_replace_nodir() {
  local source=$1 target=$2 identity target_identity nested
  _dot_detect_move_tool || return 1
  identity=$(_dot_path_identity "$source") || return 1
  if [[ $DOT_MOVE_MODE == T ]]; then
    "$DOT_MOVE_BIN" -fT -- "$source" "$target" 2>/dev/null || true
  else
    "$DOT_MOVE_BIN" -fh "$source" "$target" 2>/dev/null || true
  fi
  target_identity=$(_dot_path_identity "$target" 2>/dev/null || true)
  [[ $target_identity == "$identity" ]] && return 0
  nested=$target/${source##*/}
  if [[ -d $target && ! -L $target &&
    $(_dot_path_identity "$nested" 2>/dev/null || true) == "$identity" ]]; then
    mv "$nested" "$source" 2>/dev/null || return 1
  fi
  return 1
}

# Publish a prepared regular file without following or nesting into a late
# directory/symlink. Existing regular output may be replaced only while its
# no-follow identity still matches the generation validated here; an absent
# destination uses exclusive publication and therefore preserves every late
# winner.
_dot_publish_prepared_regular() {
  local source=$1 target=$2 expected_identity=''
  [[ -f $source && ! -L $source ]] || return 1
  if [[ -e $target || -L $target ]]; then
    [[ -f $target && ! -L $target ]] || return 1
    expected_identity=$(_dot_path_identity "$target") || return 1
    [[ $(_dot_path_identity "$target" 2>/dev/null || true) == "$expected_identity" ]] ||
      return 1
    _dot_move_replace_nodir "$source" "$target"
  else
    _dot_move_noreplace "$source" "$target"
  fi
}
