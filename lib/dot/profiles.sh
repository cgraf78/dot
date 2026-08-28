# shellcheck shell=bash
# Strict, data-only profile definitions and user/host selector resolution.

# shellcheck source=profile-format.sh
. "${BASH_SOURCE[0]%/*}/profile-format.sh"

# shellcheck disable=SC2034 # Published internal state consumed by other modules.
DOT_PROFILES_PRESENT=0
SELECTED_PROFILE=
DOT_PROFILE_SELECTION_STATE=legacy
DOT_PROFILE_CURRENT_USER=
DOT_PROFILE_CURRENT_HOST=
INCLUDED_PROFILES=()
SELECTED_OVERLAY_NAMES=()
DOT_PROFILE_SELECTOR_MATCHES=()
DOT_PROFILE_SELECTOR_RECORDS=()
_DOT_PROFILE_SELECTOR_CANDIDATES=()

declare -A _DOT_PROFILE_PARENTS=()
declare -A _DOT_PROFILE_OVERLAYS=()
declare -A _DOT_PROFILE_STATES=()
declare -A _DOT_PROFILE_NAMES=()

_dot_profile_error() {
  # shellcheck disable=SC2034 # Published internal state consumed by doctor.
  DOT_PROFILE_CONFIGURATION_ERROR=$*
  printf 'dot: profile: %s\n' "$*" >&2
  return 1
}

_dot_profile_value_safe() {
  local value=${1:-}
  [[ $value != *'|'* && $value != *$'\t'* &&
    $value != *$'\n'* && $value != *$'\r'* ]]
}

_dot_profile_file_safe() {
  local path=$1 size

  [[ -f $path && ! -L $path ]] ||
    _dot_profile_error "not a regular file: $path" || return
  size=$(LC_ALL=C wc -c <"$path" 2>/dev/null | tr -d '[:space:]') ||
    _dot_profile_error "cannot size file: $path" || return
  [[ $size =~ ^[0-9]+$ && $size -le 65536 ]] ||
    _dot_profile_error "file exceeds 65536 bytes: $path" || return
  LC_ALL=C od -An -t u1 "$path" 2>/dev/null |
    awk '{ for (i = 1; i <= NF; i++) if (($i < 32 && $i != 10) || $i == 127) exit 1 }' ||
    _dot_profile_error "contains control bytes: $path"
}

_dot_profile_private_path_safe() {
  local path=$1 mode uid

  [[ -O $path && ! -L $path ]] || return 1
  if read -r uid mode < <(command stat -c '%u %a' "$path" 2>/dev/null); then
    :
  elif read -r uid mode < <(command stat -f '%u %Lp' "$path" 2>/dev/null); then
    :
  else
    return 1
  fi
  [[ $uid == "$EUID" && $mode != *[!0-7]* ]] || return 1
  (((8#$mode & 077) == 0))
}

_dot_profile_owned_directory_safe() {
  local path=$1 uid
  [[ -d $path && ! -L $path ]] || return 1
  uid=$(command stat -c '%u' "$path" 2>/dev/null ||
    command stat -f '%u' "$path" 2>/dev/null) || return 1
  [[ $uid == "$EUID" ]]
}

_dot_profile_list_validate() {
  local value=$1 kind=$2 item
  local -a items=()

  [[ -n $value ]] || return 1
  IFS=, read -r -a items <<<"$value"
  ((${#items[@]} > 0)) || return 1
  for item in "${items[@]}"; do
    _dot_profile_identifier_valid "$item" || return 1
    [[ $kind != overlay || $item != dotfiles ]] || return 1
  done
}

_dot_profile_parse_definition() {
  local path=$1 name=$2 line key value line_number=0
  local seen_version=0 seen_profiles=0 seen_overlays=0 saw_setting=0
  local parents='' overlays=''

  _dot_profile_file_safe "$path" || return
  while IFS= read -r line || [[ -n $line ]]; do
    line_number=$((line_number + 1))
    case $line in
      '' | \#*) continue ;;
      *\\) _dot_profile_error "$path:$line_number uses a continuation" || return ;;
      *=*) ;;
      *) _dot_profile_error "$path:$line_number is not key=value" || return ;;
    esac
    key=${line%%=*}
    value=${line#*=}
    [[ -n $key && $key != *[!a-z_]* ]] ||
      _dot_profile_error "$path:$line_number has an invalid key" || return
    _dot_profile_value_safe "$value" ||
      _dot_profile_error "$path:$line_number has an unsafe value" || return
    if [[ $saw_setting -eq 0 && $key != version ]]; then
      _dot_profile_error "$path: version=1 must be the first setting" || return
    fi
    saw_setting=1
    case $key in
      version)
        [[ $seen_version -eq 0 ]] ||
          _dot_profile_error "$path: duplicate version" || return
        seen_version=1
        [[ $value == 1 ]] ||
          _dot_profile_error "$path: unsupported version: $value" || return
        ;;
      profiles)
        [[ $seen_profiles -eq 0 ]] ||
          _dot_profile_error "$path: duplicate profiles" || return
        seen_profiles=1
        _dot_profile_list_validate "$value" profile ||
          _dot_profile_error "$path: invalid profiles list" || return
        parents=$value
        ;;
      overlays)
        [[ $seen_overlays -eq 0 ]] ||
          _dot_profile_error "$path: duplicate overlays" || return
        seen_overlays=1
        _dot_profile_list_validate "$value" overlay ||
          _dot_profile_error "$path: invalid overlays list" || return
        overlays=$value
        ;;
      *) _dot_profile_error "$path: unknown key: $key" || return ;;
    esac
  done <"$path"

  [[ $seen_version -eq 1 ]] || _dot_profile_error "$path: missing version=1" || return
  [[ $seen_profiles -eq 1 || $seen_overlays -eq 1 ]] ||
    _dot_profile_error "$path: profile has no members" || return
  _DOT_PROFILE_PARENTS["$name"]=$parents
  _DOT_PROFILE_OVERLAYS["$name"]=$overlays
  _DOT_PROFILE_NAMES["$name"]=1
}

_dot_profile_append_unique() {
  local array_name=$1 value=$2 existing
  case $array_name in
    INCLUDED_PROFILES)
      for existing in "${INCLUDED_PROFILES[@]+"${INCLUDED_PROFILES[@]}"}"; do
        [[ $existing != "$value" ]] || return 0
      done
      INCLUDED_PROFILES+=("$value")
      ;;
    SELECTED_OVERLAY_NAMES)
      for existing in "${SELECTED_OVERLAY_NAMES[@]+"${SELECTED_OVERLAY_NAMES[@]}"}"; do
        [[ $existing != "$value" ]] || return 0
      done
      SELECTED_OVERLAY_NAMES+=("$value")
      ;;
    *) return 2 ;;
  esac
}

_dot_profile_expand() {
  local name=$1 parent overlay
  local -a parents=() overlays=()

  [[ -n ${_DOT_PROFILE_NAMES[$name]+x} ]] ||
    _dot_profile_error "unknown profile: $name" || return
  case ${_DOT_PROFILE_STATES[$name]:-} in
    visiting) _dot_profile_error "profile inclusion cycle at: $name" || return ;;
    resolved) return 0 ;;
  esac
  _DOT_PROFILE_STATES["$name"]=visiting
  if [[ -n ${_DOT_PROFILE_PARENTS[$name]} ]]; then
    IFS=, read -r -a parents <<<"${_DOT_PROFILE_PARENTS[$name]}"
    for parent in "${parents[@]}"; do
      _dot_profile_expand "$parent" || return
    done
  fi
  _dot_profile_append_unique INCLUDED_PROFILES "$name"
  if [[ -n ${_DOT_PROFILE_OVERLAYS[$name]} ]]; then
    IFS=, read -r -a overlays <<<"${_DOT_PROFILE_OVERLAYS[$name]}"
    for overlay in "${overlays[@]}"; do
      _dot_profile_append_unique SELECTED_OVERLAY_NAMES "$overlay"
    done
  fi
  _DOT_PROFILE_STATES["$name"]=resolved
}

_dot_profile_flatten() {
  local name=$1

  INCLUDED_PROFILES=()
  SELECTED_OVERLAY_NAMES=()
  _DOT_PROFILE_STATES=()
  _dot_profile_expand "$name" || return
  ((${#SELECTED_OVERLAY_NAMES[@]} > 0)) ||
    _dot_profile_error "profile expansion is empty: $name"
}

_dot_profiles_load() {
  local profiles_dir=${1:-} file name profile
  local nullglob_was_set=0

  DOT_PROFILES_PRESENT=0
  SELECTED_PROFILE=
  DOT_PROFILE_SELECTION_STATE=legacy
  INCLUDED_PROFILES=()
  SELECTED_OVERLAY_NAMES=()
  DOT_PROFILE_SELECTOR_MATCHES=()
  DOT_PROFILE_SELECTOR_RECORDS=()
  unset DOT_PROFILE_CONFIGURATION_ERROR
  _DOT_PROFILE_PARENTS=()
  _DOT_PROFILE_OVERLAYS=()
  _DOT_PROFILE_STATES=()
  _DOT_PROFILE_NAMES=()

  if [[ -z $profiles_dir ]]; then
    dot_xdg_path config dot/profiles.d || return
    profiles_dir=$REPLY
  fi
  [[ -e $profiles_dir || -L $profiles_dir ]] || return 0
  [[ -d $profiles_dir && ! -L $profiles_dir ]] ||
    _dot_profile_error "not a directory: $profiles_dir" || return
  DOT_PROFILES_PRESENT=1

  shopt -q nullglob && nullglob_was_set=1
  shopt -s nullglob
  for file in "$profiles_dir"/*.conf; do
    name=${file##*/}
    name=${name%.conf}
    _dot_profile_identifier_valid "$name" || {
      [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
      _dot_profile_error "invalid profile filename: ${file##*/}"
      return
    }
    _dot_profile_parse_definition "$file" "$name" || {
      [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
      return 1
    }
  done
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob

  [[ -n ${_DOT_PROFILE_NAMES[base]+x} ]] ||
    _dot_profile_error 'profiles.d must define base' || return
  _dot_profile_identifier_valid "${DOT_DEFAULT_PROFILE:-base}" ||
    _dot_profile_error "invalid default profile: ${DOT_DEFAULT_PROFILE:-}" || return
  [[ -n ${_DOT_PROFILE_NAMES[${DOT_DEFAULT_PROFILE:-base}]+x} ]] ||
    _dot_profile_error \
      "unknown default profile: ${DOT_DEFAULT_PROFILE:-base}" || return
  for profile in "${!_DOT_PROFILE_NAMES[@]}"; do
    _dot_profile_flatten "$profile" || return
  done
  INCLUDED_PROFILES=()
  SELECTED_OVERLAY_NAMES=()
  _DOT_PROFILE_STATES=()
}

_dot_profile_host_normalize() {
  local host=$1
  host=${host%.}
  [[ -n $host && $host =~ ^[A-Za-z0-9][A-Za-z0-9.-]*$ ]] || return 1
  REPLY=$(printf '%s' "$host" | LC_ALL=C tr '[:upper:]' '[:lower:]') || return 1
}

_dot_profile_user_valid() {
  [[ ${1:-} =~ ^[A-Za-z_][A-Za-z0-9_.-]*$ ]]
}

_dot_profile_selector_parse() {
  local path=$1 source_class=$2 line key value line_number=0
  local seen_version=0 seen_user=0 seen_host=0 seen_profile=0 saw_setting=0
  local user='' host='' profile=''

  _dot_profile_file_safe "$path" || return
  if [[ $source_class == local ]]; then
    _dot_profile_private_path_safe "$path" ||
      _dot_profile_error "unsafe machine-local selector file: $path" || return
  fi
  while IFS= read -r line || [[ -n $line ]]; do
    line_number=$((line_number + 1))
    case $line in
      '' | \#*) continue ;;
      *\\) _dot_profile_error "$path:$line_number uses a continuation" || return ;;
      *=*) ;;
      *) _dot_profile_error "$path:$line_number is not key=value" || return ;;
    esac
    key=${line%%=*}
    value=${line#*=}
    [[ -n $key && $key != *[!a-z_]* ]] ||
      _dot_profile_error "$path:$line_number has an invalid key" || return
    _dot_profile_value_safe "$value" ||
      _dot_profile_error "$path:$line_number has an unsafe value" || return
    if [[ $saw_setting -eq 0 && $key != version ]]; then
      _dot_profile_error "$path: version=1 must be the first setting" || return
    fi
    saw_setting=1
    case $key in
      version)
        [[ $seen_version -eq 0 ]] ||
          _dot_profile_error "$path: duplicate version" || return
        seen_version=1
        [[ $value == 1 ]] ||
          _dot_profile_error "$path: unsupported version: $value" || return
        ;;
      user)
        [[ $seen_user -eq 0 ]] || _dot_profile_error "$path: duplicate user" || return
        seen_user=1
        _dot_profile_user_valid "$value" ||
          _dot_profile_error "$path: invalid user: $value" || return
        user=$value
        ;;
      host)
        [[ $seen_host -eq 0 ]] || _dot_profile_error "$path: duplicate host" || return
        seen_host=1
        _dot_profile_host_normalize "$value" ||
          _dot_profile_error "$path: invalid host: $value" || return
        host=$REPLY
        ;;
      profile)
        [[ $seen_profile -eq 0 ]] ||
          _dot_profile_error "$path: duplicate profile" || return
        seen_profile=1
        _dot_profile_identifier_valid "$value" ||
          _dot_profile_error "$path: invalid profile: $value" || return
        profile=$value
        ;;
      *) _dot_profile_error "$path: unknown key: $key" || return ;;
    esac
  done <"$path"
  [[ $seen_version -eq 1 ]] || _dot_profile_error "$path: missing version=1" || return
  [[ $seen_profile -eq 1 ]] || _dot_profile_error "$path: missing profile" || return
  [[ $seen_user -eq 1 || $seen_host -eq 1 ]] ||
    _dot_profile_error "$path: selector requires user or host" || return
  [[ -n ${_DOT_PROFILE_NAMES[$profile]+x} ]] ||
    _dot_profile_error "$path: unknown profile: $profile" || return
  REPLY="$user|$host|$profile"
}

_dot_profile_read_selector_dir() {
  local source_class=$1 directory=$2 file user host profile matched=false
  local specificity
  local nullglob_was_set=0

  [[ -n $directory ]] || return 0
  [[ -e $directory || -L $directory ]] || return 0
  [[ -d $directory && ! -L $directory ]] ||
    _dot_profile_error "not a selector directory: $directory" || return
  if [[ $source_class == local ]]; then
    _dot_profile_private_path_safe "$directory" ||
      _dot_profile_error "unsafe machine-local selector directory: $directory" || return
  fi
  shopt -q nullglob && nullglob_was_set=1
  shopt -s nullglob
  for file in "$directory"/*.conf; do
    _dot_profile_selector_parse "$file" "$source_class" || {
      [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
      return 1
    }
    IFS='|' read -r user host profile <<<"$REPLY"
    matched=false
    if [[ -z $user || $user == "$DOT_PROFILE_CURRENT_USER" ]]; then
      if [[ -z $host || $host == "$DOT_PROFILE_CURRENT_HOST" ]]; then
        matched=true
      fi
    fi
    DOT_PROFILE_SELECTOR_RECORDS+=(
      "$source_class|$file|$user|$host|$profile|$matched"
    )
    if [[ $matched == true ]]; then
      DOT_PROFILE_SELECTOR_MATCHES+=(
        "$source_class:${file##*/}:$profile"
      )
      specificity=0
      [[ -z $user ]] || specificity=$((specificity + 1))
      [[ -z $host ]] || specificity=$((specificity + 1))
      _DOT_PROFILE_SELECTOR_CANDIDATES+=("$specificity|$profile")
    fi
  done
  [[ $nullglob_was_set -eq 1 ]] || shopt -u nullglob
}

_dot_profile_choose_selector() {
  local candidate specificity profile
  local selected_specificity=0

  SELECTED_PROFILE=
  for candidate in "${_DOT_PROFILE_SELECTOR_CANDIDATES[@]}"; do
    IFS='|' read -r specificity profile <<<"$candidate"
    if ((specificity > selected_specificity)); then
      selected_specificity=$specificity
    fi
  done
  for candidate in "${_DOT_PROFILE_SELECTOR_CANDIDATES[@]}"; do
    IFS='|' read -r specificity profile <<<"$candidate"
    ((specificity == selected_specificity)) || continue
    if [[ -z $SELECTED_PROFILE ]]; then
      SELECTED_PROFILE=$profile
    elif [[ $SELECTED_PROFILE != "$profile" ]]; then
      DOT_PROFILE_SELECTION_STATE=conflict
      _dot_profile_error \
        "equally specific selectors choose $SELECTED_PROFILE and $profile"
      return
    fi
  done
}

_dot_profile_resolve() {
  local root_dir=${1:-} local_dir=${2:-}
  local identity host
  local personal_dir
  shift 2 || true

  [[ $DOT_PROFILES_PRESENT -eq 1 ]] || return 0
  identity=$(id -un 2>/dev/null) || _dot_profile_error 'cannot determine current user' || return
  _dot_profile_user_valid "$identity" ||
    _dot_profile_error "invalid current user: $identity" || return
  host=$(_dot_host) ||
    _dot_profile_error 'cannot determine current short hostname' || return
  _dot_profile_host_normalize "$host" ||
    _dot_profile_error "invalid current short hostname: $host" || return
  DOT_PROFILE_CURRENT_USER=$identity
  DOT_PROFILE_CURRENT_HOST=$REPLY
  SELECTED_PROFILE=
  DOT_PROFILE_SELECTION_STATE=implicit-default
  DOT_PROFILE_SELECTOR_MATCHES=()
  DOT_PROFILE_SELECTOR_RECORDS=()
  _DOT_PROFILE_SELECTOR_CANDIDATES=()

  _dot_profile_read_selector_dir root "$root_dir" || return
  _dot_profile_read_selector_dir local "$local_dir" || return
  for personal_dir in "$@"; do
    _dot_profile_read_selector_dir personal "$personal_dir" || return
  done
  _dot_profile_choose_selector || return
  if [[ -z $SELECTED_PROFILE ]]; then
    SELECTED_PROFILE=${DOT_DEFAULT_PROFILE:-base}
  else
    # shellcheck disable=SC2034 # Published internal state consumed by doctor.
    DOT_PROFILE_SELECTION_STATE=agreed-match
  fi
  _dot_profile_flatten "$SELECTED_PROFILE"
}

_dot_profiles_load_default() {
  dot_xdg_path config dot/profiles.d || return
  _dot_profiles_load "$REPLY"
}

_dot_profile_select_base() {
  [[ $DOT_PROFILES_PRESENT -eq 1 ]] || return 0
  SELECTED_PROFILE=base
  # shellcheck disable=SC2034 # Published internal state consumed by doctor.
  DOT_PROFILE_SELECTION_STATE=phase-one
  DOT_PROFILE_SELECTOR_MATCHES=()
  DOT_PROFILE_SELECTOR_RECORDS=()
  _dot_profile_flatten base
}

_dot_profile_resolve_default() {
  local entry path dot_dir selector_dir
  local -a personal_dirs=()

  dot_xdg_path config dot/profile-selectors.d || return
  local root_dir=$REPLY
  dot_xdg_path config dot/profile-selectors.local.d || return
  local local_dir=$REPLY
  for entry in "${ACTIVE_OVERLAYS[@]+"${ACTIVE_OVERLAYS[@]}"}"; do
    IFS='|' read -r _ path _ <<<"$entry"
    dot_dir=$path/dot
    selector_dir=$dot_dir/profile-selectors.d
    [[ -e $dot_dir || -L $dot_dir ]] || continue
    _dot_profile_owned_directory_safe "$path" &&
      _dot_profile_owned_directory_safe "$dot_dir" ||
      _dot_profile_error "unsafe personal selector ancestry: $dot_dir" || return
    [[ -e $selector_dir || -L $selector_dir ]] || continue
    _dot_profile_owned_directory_safe "$selector_dir" ||
      _dot_profile_error "unsafe personal selector directory: $selector_dir" || return
    personal_dirs+=("$selector_dir")
  done
  _dot_profile_resolve "$root_dir" "$local_dir" \
    "${personal_dirs[@]+"${personal_dirs[@]}"}"
}
