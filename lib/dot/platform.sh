# shellcheck shell=bash
# Provider-independent platform, host, and generic executable predicates.

_is_wsl() {
  [[ -n ${WSL_DISTRO_NAME:-} || -n ${WSL_INTEROP:-} ]] && return 0
  [[ -r /proc/sys/kernel/osrelease ]] &&
    grep -qi microsoft /proc/sys/kernel/osrelease
}

_dot_platform() {
  local value
  if _is_wsl; then
    printf 'wsl\n'
    return
  fi
  value=$(uname -s 2>/dev/null) || return 1
  value=${value,,}
  [[ $value == darwin ]] && value=macos
  printf '%s\n' "$value"
}

_dot_host() {
  local value
  value=$(hostname -s 2>/dev/null || hostname 2>/dev/null) || return 1
  printf '%s\n' "${value,,}"
}

_dot_match_specs() {
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

dot_platform_match() {
  local platform
  [[ $# -eq 1 ]] || return 2
  platform=$(_dot_platform) || return 1
  _dot_match_specs "$1" exact "$platform" && return 0
  # Termux is Linux at the kernel boundary and Android at the userspace
  # boundary. Preserve both durable identities, matching the provider ABI.
  if [[ -n ${PREFIX:-} && $PREFIX == */com.termux/* ]]; then
    _dot_match_specs "$1" exact android && return 0
  fi
  return 1
}

dot_host_match() {
  local host
  [[ $# -eq 1 ]] || return 2
  host=$(_dot_host) || return 1
  _dot_match_specs "$1" lowercase "$host"
}

_dot_tool_present() {
  [[ $# -eq 1 && -n $1 ]] || return 2
  case $1 in
    */*) [[ -e $1 ]] ;;
    *) command -v "$1" >/dev/null 2>&1 ;;
  esac
}

_require_sudo() {
  [[ $(id -u) -eq 0 ]] && return 0
  sudo -n true 2>/dev/null && return 0
  [[ ${DOT_QUIET:-0} -eq 1 ]] && return 1
  sudo true 2>/dev/null
}
