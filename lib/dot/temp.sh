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

# Internal Git policy must not inherit a caller's selected repository, object
# store, hash default, or configuration. Keep that shared isolation boundary in
# one place for content comparison and durable overlay identities.
_dot_sanitized_git() (
  unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY
  unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_INDEX_FILE GIT_CONFIG
  unset GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_COUNT
  unset GIT_CONFIG_PARAMETERS GIT_CONFIG_NOSYSTEM GIT_DEFAULT_HASH
  export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
  command git "$@"
)

# Git is already a required Dot dependency and provides a raw, filter-free
# content hash. Keep exact-content checks on that shared baseline so minimal
# distributions do not also need a standalone `cmp` or `diff` executable.
_dot_hash_object() {
  local source_root=${DOT_SOURCE_ROOT:-$PWD}

  _dot_sanitized_git -C "$source_root" hash-object "$@"
}

_dot_hash_pair_equal() {
  local hashes=$1 first second

  [[ $hashes == *$'\n'* ]] || return 1
  first=${hashes%%$'\n'*}
  second=${hashes#*$'\n'}
  [[ $second != *$'\n'* ]] || return 1
  [[ $first =~ ^[0-9a-f]{40}$ || $first =~ ^[0-9a-f]{64}$ ]] || return 1
  [[ $second == "$first" ]]
}

_dot_files_equal() {
  local hashes

  hashes=$(_dot_hash_object --no-filters -- "$1" "$2" 2>/dev/null) || return 1
  _dot_hash_pair_equal "$hashes"
}

_dot_stdin_matches_file() {
  local hashes

  hashes=$(_dot_hash_object --no-filters --stdin -- "$1" 2>/dev/null) || return 1
  _dot_hash_pair_equal "$hashes"
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
