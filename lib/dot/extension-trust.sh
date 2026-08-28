# shellcheck shell=bash
# Shared trust checks for executable extension entry points and support files.
# The overlay manifest, link-target, and checkout identity functions are the
# canonical read-only implementations from repos/config.sh and
# repos/overlays.sh. Workers retain only that small whitelist, so authorization
# cannot drift from publication while mutation/coordinator internals stay out
# of the client process.

_dot_extension_stat_fields() {
  local path=$1 output

  if output=$(command stat -c '%u %a %h' "$path" 2>/dev/null); then
    :
  elif output=$(command stat -f '%u %Lp %l' "$path" 2>/dev/null); then
    :
  else
    return 1
  fi
  read -r REPLY_UID REPLY_MODE REPLY_LINKS <<<"$output"
  [[ $REPLY_UID == "$EUID" && $REPLY_MODE != *[!0-7]* ]] || return 1
  (((8#$REPLY_MODE & 022) == 0))
}

_dot_extension_file_stat() {
  local REPLY_UID REPLY_MODE REPLY_LINKS

  _dot_extension_stat_fields "$1" || return 1
  [[ $REPLY_LINKS == 1 ]]
}

_dot_extension_directory_stat() {
  local REPLY_UID REPLY_MODE REPLY_LINKS

  _dot_extension_stat_fields "$1"
}

_dot_extension_root_validate() {
  local root=${DOT_EXTENSIONS_DIR:-}

  case $root in
    '' | / | */ | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
      return 1
      ;;
    /*) ;;
    *) return 1 ;;
  esac
  [[ -d $root && ! -L $root ]] || return 1
  _dot_extension_directory_stat "$root"
}

_dot_extension_parent_components_validate() {
  local path=$1 root=${DOT_EXTENSIONS_DIR:-} relative parent component current
  local -a components=()

  _dot_extension_root_validate || return 1
  case $path in
    "$root"/*) relative=${path#"$root"/} ;;
    *) return 1 ;;
  esac
  case $relative in
    '' | /* | . | .. | ./* | ../* | */./* | */../* | */. | */.. | */ | *//* | *$'\n'* | *$'\r'*)
      return 1
      ;;
  esac
  [[ $relative == */* ]] || return 0
  parent=${relative%/*}
  IFS=/ read -r -a components <<<"$parent"
  current=$root
  for component in "${components[@]}"; do
    current=$current/$component
    [[ -d $current && ! -L $current ]] || return 1
    _dot_extension_directory_stat "$current" || return 1
  done
}

_dot_extension_directory_validate() {
  local path=$1

  _dot_extension_parent_components_validate "$path" || return 1
  [[ -d $path && ! -L $path ]] || return 1
  _dot_extension_directory_stat "$path"
}

_dot_extension_owned_parent_components_validate() {
  local root=$1 path=$2 relative parent component current
  local -a components=()

  [[ -d $root && ! -L $root ]] || return 1
  _dot_extension_directory_stat "$root" || return 1
  case $path in
    "$root"/*) relative=${path#"$root"/} ;;
    *) return 1 ;;
  esac
  [[ $relative == */* ]] || return 0
  parent=${relative%/*}
  IFS=/ read -r -a components <<<"$parent"
  current=$root
  for component in "${components[@]}"; do
    current=$current/$component
    [[ -d $current && ! -L $current ]] || return 1
    _dot_extension_directory_stat "$current" || return 1
  done
}

_dot_extension_symlink_authorized() {
  local path=$1 rel line owner='' target='' entry name overlay_path url sync
  local source_root resolved expected REPLY_REL REPLY_OWNER REPLY_TARGET

  if [[ $HOME == / ]]; then
    case $path in
      /*) rel=${path#/} ;;
      *) return 1 ;;
    esac
  else
    case $path in
      "$HOME"/*) rel=${path#"$HOME"/} ;;
      *) return 1 ;;
    esac
  fi
  [[ -L $path && -f ${DOT_OVERLAY_MANIFEST:-} ]] || return 1
  _overlay_manifest_safe "$DOT_OVERLAY_MANIFEST" || return 1
  while IFS= read -r line || [[ -n $line ]]; do
    _overlay_parse_manifest_record "$line" || return 1
    if [[ $REPLY_REL == "$rel" && $(command readlink "$path") == "$REPLY_TARGET" ]]; then
      owner=$REPLY_OWNER
      target=$REPLY_TARGET
      break
    fi
  done <"$DOT_OVERLAY_MANIFEST"
  [[ -n $owner && -n $target ]] || return 1

  # A private manifest records prior ownership; only a currently active,
  # identity-matching Git checkout may still supply executable extension code.
  for entry in "${OVERLAYS[@]+"${OVERLAYS[@]}"}"; do
    IFS='|' read -r name overlay_path url _ _ sync <<<"$entry"
    [[ $name == "$owner" ]] || continue
    sync=${sync:-git}
    [[ $sync == git ]] || return 1
    _overlay_checkout_matches "$overlay_path" "$url" || return 1
    [[ -d $overlay_path && ! -L $overlay_path ]] || return 1
    _dot_extension_directory_stat "$overlay_path" || return 1
    [[ -d $overlay_path/home && ! -L $overlay_path/home ]] || return 1
    _dot_extension_directory_stat "$overlay_path/home" || return 1
    _overlay_link_target "$rel" "$owner"
    expected=$REPLY
    [[ $target == "$expected" ]] || return 1
    source_root=$(cd -P -- "$overlay_path/home" 2>/dev/null && pwd -P) ||
      return 1
    resolved=$(command realpath "$path" 2>/dev/null) || return 1
    case $resolved in
      "$source_root"/*)
        _dot_extension_owned_parent_components_validate \
          "$source_root" "$resolved" || return 1
        return 0
        ;;
      *) return 1 ;;
    esac
  done
  return 1
}

_dot_extension_file_validate() {
  local path=$1 resolved

  _dot_extension_parent_components_validate "$path" || return 1
  [[ -r $path ]] || return 1
  if [[ -L $path ]]; then
    _dot_extension_symlink_authorized "$path" || return 1
    resolved=$(command realpath "$path" 2>/dev/null) || return 1
  else
    [[ -f $path ]] || return 1
    resolved=$path
  fi
  [[ -f $resolved && ! -L $resolved ]] || return 1
  _dot_extension_file_stat "$resolved"
}

# Validate the one repository-owned lifecycle entry point that is deliberately
# not linked into HOME. A retiring overlay is no longer active, so its saved
# Git identity is the authority; accepting extension-dir symlinks here would
# let the new profile silently replace the cleanup code for the old one.
_dot_profile_deactivation_validate() {
  local record=${1:-} script=${2:-}
  local name path url _descriptor _optional sync

  [[ $# -eq 2 ]] || return 2
  _dot_overlay_record_validate "$record" || return 1
  IFS='|' read -r name path url _descriptor _optional sync <<<"$record"
  [[ $sync == git && $path == "$HOME/.dotfiles-$name" ]] || return 1
  [[ $script == "$path/dot/profile-deactivate" ]] || return 1
  _overlay_checkout_matches "$path" "$url" || return 1
  [[ -f $script && ! -L $script ]] || return 1
  _dot_extension_owned_parent_components_validate "$path" "$script" ||
    return 1
  _dot_extension_file_stat "$script"
}

# Resolve a support file from the already validated retiring checkout. The
# worker publishes the checkout root as readonly state only after validating
# the saved Git identity and fixed deactivation entry point.
dot_retiring_overlay_file() {
  local relative=${1:-} root=${DOT_RETIRING_OVERLAY_ROOT:-} path

  [[ $# -eq 1 ]] || return 2
  case $relative in
    '' | /* | . | .. | ./* | ../* | */./* | */../* | */. | */.. | */ | \
      *//* | *$'\n'* | *$'\r'*)
      return 2
      ;;
  esac
  [[ -n $root && -d $root && ! -L $root ]] || return 1
  path=$root/$relative
  _dot_extension_owned_parent_components_validate "$root" "$path" ||
    return 1
  [[ -f $path && ! -L $path && -r $path ]] || return 1
  _dot_extension_file_stat "$path" || return 1
  REPLY=$path
}
