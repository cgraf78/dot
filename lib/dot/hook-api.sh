# shellcheck shell=bash
# Public extension API 1 helpers loaded into isolated hook workers.

if ! declare -F _dot_extension_file_validate >/dev/null 2>&1; then
  # shellcheck source=extension-trust.sh
  . "${BASH_SOURCE[0]%/*}/extension-trust.sh"
fi

dot_hook_source() {
  local _dot_hook_source_relative=${1:-} _dot_hook_source_path

  [[ $# -eq 1 ]] || return 2
  case $_dot_hook_source_relative in
    '' | /* | . | .. | ./* | ../* | */./* | */../* | */. | */.. | */ | *//* | *$'\n'* | *$'\r'*)
      return 2
      ;;
  esac
  _dot_hook_source_path=$DOT_EXTENSIONS_DIR/$_dot_hook_source_relative
  readonly _dot_hook_source_relative _dot_hook_source_path
  _dot_extension_file_validate "$_dot_hook_source_path" || return 1
  # Support code shares the worker's global scope, but it must observe the same
  # empty positional-parameter baseline as the entry hook. Top-level `local`
  # remains inappropriate in sourced support modules because the loader itself
  # is necessarily a function.
  set --
  # shellcheck source=/dev/null
  . "$_dot_hook_source_path" || return 1
}

dot_hook_family() {
  [[ $# -eq 1 ]] || return 2
  _merge_hook_family "$1"
}

dot_hook_family_files() {
  [[ $# -eq 1 ]] || return 2
  _merge_hook_family_files "$1"
}

dot_hook_family_files_matching() {
  [[ $# -ge 1 ]] || return 2
  _merge_hook_family_files_matching "$@"
}

dot_hook_family_relpath() {
  [[ $# -eq 2 ]] || return 2
  _merge_hook_family_relpath "$@"
}

dot_hook_family_marker_name() {
  local relative

  [[ $# -eq 2 ]] || return 2
  relative=$(_merge_hook_family_relpath "$1" "$2") || return
  printf '%s\n' "${relative//\//_}"
}

dot_family_relpath() {
  [[ $# -eq 2 ]] || return 2
  _merge_hook_family_relpath "$1" "$2"
}

dot_expand_home() {
  [[ $# -eq 1 ]] || return 2
  _merge_hook_expand_home "$1"
}

dot_sibling_tmp_for() {
  [[ $# -eq 1 ]] || return 2
  _dot_sibling_tmp_for "$1"
}

dot_write_text_if_changed() {
  [[ $# -eq 2 ]] || return 2
  _merge_hook_write_text_if_changed "$1" "$2"
}

dot_commit_tmp() {
  [[ $# -eq 2 ]] || return 2
  _merge_hook_commit_tmp "$1" "$2"
}

dot_json_available() {
  [[ $# -eq 0 ]] || return 2
  _merge_hook_jq_available
}

dot_json_layer() {
  [[ $# -eq 4 ]] || return 2
  _merge_hook_jq_layer "$@"
}

dot_managed_block_build() {
  [[ $# -eq 3 ]] || return 2
  _mb_build "$@"
}

dot_managed_block_strip() {
  [[ $# -eq 2 ]] || return 2
  _mb_strip "$@"
}

dot_managed_block_strip_family() {
  [[ $# -eq 2 ]] || return 2
  _mb_strip_family "$@"
}

dot_managed_block_merge() {
  [[ $# -ge 1 ]] || return 2
  _mb_merge "$@"
}

dot_managed_block_merge_family() {
  [[ $# -ge 2 ]] || return 2
  _mb_merge_family "$@"
}

dot_tool_present() {
  [[ $# -eq 1 ]] || return 2
  [[ -n $1 ]] || return 2
  case $1 in
    */*) [[ -e $1 ]] ;;
    *) command -v "$1" >/dev/null 2>&1 ;;
  esac
}

_dot_hook_platform() {
  local value

  if [[ -n ${WSL_DISTRO_NAME:-} || -n ${WSL_INTEROP:-} ]] ||
    { [[ -r /proc/sys/kernel/osrelease ]] &&
      grep -qi microsoft /proc/sys/kernel/osrelease; }; then
    printf 'wsl\n'
    return
  fi
  value=$(uname -s 2>/dev/null) || return 1
  value=${value,,}
  [[ $value == darwin ]] && value=macos
  printf '%s\n' "$value"
}

_dot_hook_host() {
  local value

  value=$(hostname -s 2>/dev/null || hostname 2>/dev/null) || return 1
  printf '%s\n' "${value,,}"
}

_dot_hook_match_specs() {
  local spec=$1 case_mode=$2 current=$3 item normalized
  local has_include=false
  local -a items=()

  [[ -n $spec ]] || return 0
  IFS=, read -r -a items <<<"$spec"
  for item in "${items[@]}"; do
    [[ -n $item ]] || continue
    if [[ $case_mode == lowercase ]]; then
      normalized=${item,,}
    else
      normalized=$item
    fi
    [[ $normalized == '!'* ]] || has_include=true
    [[ $normalized == "!$current" ]] && return 1
  done
  [[ $has_include == false ]] && return 0
  for item in "${items[@]}"; do
    [[ -n $item ]] || continue
    [[ $case_mode == lowercase ]] && item=${item,,}
    [[ $item == "$current" ]] && return 0
  done
  return 1
}

dot_hook_platform_match() {
  local platform

  [[ $# -eq 1 ]] || return 2
  platform=$(_dot_hook_platform) || return 1
  _dot_hook_match_specs "$1" exact "$platform" && return 0
  if [[ -n ${PREFIX:-} && $PREFIX == */com.termux/* ]]; then
    _dot_hook_match_specs "$1" exact android && return 0
  fi
  return 1
}

dot_hook_host_match() {
  local host

  [[ $# -eq 1 ]] || return 2
  host=$(_dot_hook_host) || return 1
  _dot_hook_match_specs "$1" lowercase "$host"
}

dot_hook_log() {
  _log "$@"
}

dot_hook_warn() {
  _warn "$@"
}
