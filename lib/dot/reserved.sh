# shellcheck shell=bash
# Dynamic control-plane paths that client repositories and overlays may not own.

_dot_path_within() {
  [[ "$1" == "$2" || "$1" == "$2"/* ]]
}

_dot_init_recovery_path_reserved() {
  local path=$1 relative component
  local -a components=()

  [[ -n ${HOME:-} && $path == /* ]] || return 1
  if [[ $HOME == / ]]; then
    relative=${path#/}
  else
    case $path in
      "$HOME"/*) relative=${path#"$HOME"/} ;;
      *) return 1 ;;
    esac
  fi
  IFS=/ read -r -a components <<<"$relative"
  for component in "${components[@]}"; do
    case $component in
      .dot-init-entry.* | .dot-init-parent.* | .dot-init-delete.*)
        return 0
        ;;
    esac
  done
  return 1
}

_dot_normalize_absolute_path() {
  local input=${1:-} component output=/
  local -a parts=() normalized=()

  [[ -n $input && $input != *$'\n'* && $input != *$'\r'* ]] || return 1
  [[ $input == /* ]] || input=$PWD/$input
  IFS=/ read -r -a parts <<<"$input"
  for component in "${parts[@]}"; do
    case $component in
      '' | .) ;;
      ..)
        ((${#normalized[@]} > 0)) && unset 'normalized[${#normalized[@]}-1]'
        ;;
      *) normalized+=("$component") ;;
    esac
  done
  if ((${#normalized[@]} > 0)); then
    local joined
    printf -v joined '/%s' "${normalized[@]}"
    output=$joined
  fi
  REPLY=$output
}

_dot_physical_directory_candidate() {
  local candidate=${1:-} suffix='' part parent physical
  _dot_normalize_absolute_path "$candidate" || return 1
  candidate=$REPLY
  while [[ ! -d $candidate ]]; do
    [[ $candidate != / ]] || return 1
    part=${candidate##*/}
    [[ -n $part ]] || return 1
    suffix=/$part$suffix
    parent=${candidate%/*}
    [[ -n $parent ]] || parent=/
    [[ $parent != "$candidate" ]] || return 1
    candidate=$parent
  done
  physical=$(cd -P -- "$candidate" 2>/dev/null && pwd -P) || return 1
  if [[ $physical == / ]]; then
    REPLY=/${suffix#/}
  else
    REPLY=$physical$suffix
  fi
}

_dot_physical_leaf_candidate() {
  local path=${1:-} parent base physical_parent
  _dot_normalize_absolute_path "$path" || return 1
  path=$REPLY
  parent=${path%/*}
  base=${path##*/}
  [[ -n $parent ]] || parent=/
  _dot_physical_directory_candidate "$parent" || return 1
  physical_parent=$REPLY
  if [[ $physical_parent == / ]]; then
    REPLY=/$base
  else
    REPLY=$physical_parent/$base
  fi
  # shellcheck disable=SC2034 # Dynamically scoped outputs consumed by publishers.
  REPLY_PHYSICAL_PARENT=$physical_parent
  # shellcheck disable=SC2034 # Dynamically scoped outputs consumed by publishers.
  REPLY_PARENT_IDENTITY=$(_dot_path_identity "$physical_parent") || return 1
}

_dot_reserved_root() {
  local requested=$1 normalized physical
  _dot_normalize_absolute_path "$requested" || return 1
  normalized=$REPLY
  printf '%s\n' "$normalized"
  # BSD realpath requires the complete path to exist, while reserved roots
  # frequently name state that dot has not created yet. Resolve the deepest
  # existing directory ancestor as a portable fallback so a symlinked parent
  # cannot hide the physical control-plane path during candidate validation.
  if ! physical=$(realpath "$normalized" 2>/dev/null); then
    _dot_physical_directory_candidate "$normalized" || return 1
    physical=$REPLY
  fi
  [[ $physical == "$normalized" ]] || printf '%s\n' "$physical"
}

_dot_reserved_roots() {
  local state_home provider_state install_root checkout parent name entry path

  dot_xdg_home state || return 1
  state_home=$REPLY
  install_root=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}
  checkout=$install_root/cgraf78/dot
  parent=${checkout%/*}
  name=${checkout##*/}

  provider_state=${SHDEPS_STATE_DIR:-$state_home/shdeps}
  for path in \
    "$state_home/dot" \
    "$provider_state" \
    "$HOME/.dotfiles" \
    "$HOME/.dot-backup" \
    "$HOME/.local/bin/.dot.dot-install-stage-v1" \
    "$HOME/.local/lib/.dot.dot-install-stage-v1" \
    "$HOME/.local/bin/dot" \
    "$HOME/.local/lib/dot" \
    "$checkout" \
    "$parent/.$name.install.lock" \
    "$parent/.$name.install.transaction" \
    "$parent/.$name.shdeps-repo-transition-v1"; do
    _dot_reserved_root "$path" || return 1
  done
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r _ path _ <<<"$entry"
    [[ -n $path ]] || continue
    _dot_reserved_root "$path" || return 1
  done
  if [[ -n ${DOT_INIT_BACKUP:-} && ${DOT_INIT_BACKUP:-} != - ]]; then
    _dot_reserved_root "$DOT_INIT_BACKUP" || return 1
  fi
}

dot_path_is_reserved() {
  local path=${1:-} root checkout parent name base matched=1

  [[ $# -eq 1 && $path == /* ]] || return 2
  _dot_init_recovery_path_reserved "$path" && return 0
  while IFS= read -r root; do
    [[ -n "$root" ]] || continue
    if _dot_path_within "$path" "$root"; then
      matched=0
    fi
  done < <(_dot_reserved_roots)
  [[ $matched -eq 1 ]] || return 0

  checkout=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}/cgraf78/dot
  parent=${checkout%/*}
  name=${checkout##*/}
  case $path in
    "$parent/.$name.install.lock.owner."* | \
      "$parent/.$name.install.lock.claim."* | \
      "$parent/.$name.clone."* | \
      "$parent/.$name.publish."* | \
      "$parent/$name.tmp."* | \
      "$parent/.$name.tmp."*)
      base=${path##*/}
      [[ "$base" != */* ]]
      return
      ;;
  esac
  return 1
}

# Candidate inventories contain only leaf entries. A leaf is unsafe when it is
# inside a reserved root or when it would replace an ancestor directory on the
# route to one (for example a tracked `.local` symlink). Normal Git directory
# ancestors are absent from a recursive tree listing and remain allowed.
dot_candidate_path_is_reserved() {
  local path=${1:-} root physical matched=1

  [[ $# -eq 1 && $path == /* ]] || return 2
  while IFS= read -r root; do
    [[ -n $root ]] || continue
    if _dot_path_within "$path" "$root" || _dot_path_within "$root" "$path"; then
      matched=0
    fi
  done < <(_dot_reserved_roots)
  [[ $matched -eq 1 ]] || return 0

  # Candidate validation often runs before its parent directories exist. Map
  # the nearest existing ancestor physically and append the missing suffix;
  # parent-generation identity is required later at the mutation boundary,
  # not while inspecting an inert tree.
  if _dot_physical_directory_candidate "$path"; then
    physical=$REPLY
    _dot_init_recovery_path_reserved "$physical" && return 0
    matched=1
    while IFS= read -r root; do
      [[ -n $root ]] || continue
      if _dot_path_within "$physical" "$root" || _dot_path_within "$root" "$physical"; then
        matched=0
      fi
    done < <(_dot_reserved_roots)
    [[ $matched -eq 1 ]] || return 0
  else
    return 0
  fi

  case $path in
    "$HOME/.dotfiles-"*) return 0 ;;
  esac
  dot_path_is_reserved "$path"
}
