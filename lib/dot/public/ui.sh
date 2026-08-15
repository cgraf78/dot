#!/usr/bin/env bash
# Public presentation helpers shared by dot and client-owned integration test
# runners. Output content remains caller-owned; these functions only prevent
# basic title and summary-box styling from drifting.

dot_ui_color_hex() {
  local LC_ALL=C

  [[ $# -eq 1 ]] || return 2
  if test "$1" = green; then
    printf '#3fb950'
  elif test "$1" = red; then
    printf '#f85149'
  elif test "$1" = yellow; then
    printf '#d29922'
  elif test "$1" = magenta; then
    printf '#bc8cff'
  elif test "$1" = dim; then
    printf '#8b949e'
  else
    case $1 in
      \#[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f])
        printf '%s' "$1"
        ;;
      *) return 2 ;;
    esac
  fi
}

dot_ui_hex_to_rgb() {
  local hex LC_ALL=C

  [[ $# -eq 1 ]] || return 2
  case $1 in
    \#[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]) ;;
    *) return 2 ;;
  esac
  hex=${1#\#}
  printf '%d;%d;%d' "0x${hex:0:2}" "0x${hex:2:2}" "0x${hex:4:2}"
}

_dot_ui_gum_bin() {
  local gum_bin

  gum_bin=$(type -P gum 2>/dev/null) || return 1
  [[ -x "$gum_bin" ]] || return 1
  "$gum_bin" style --help >/dev/null 2>&1 || return 1
  REPLY=$gum_bin
}

dot_ui_has_gum() {
  REPLY=
  [[ $# -eq 0 ]] || return 2
  _dot_ui_gum_bin
}

dot_ui_title() {
  local IFS=' ' REPLY=

  [[ $# -gt 0 ]] || return 2
  if _dot_ui_gum_bin; then
    "$REPLY" style --bold --foreground 212 --border normal --padding '0 2' "$*"
  elif [[ -t 1 && -z ${NO_COLOR:-} ]]; then
    printf '\n\033[1m%s\033[0m\n\n' "$*"
  else
    printf '\n%s\n\n' "$*"
  fi
}

dot_ui_summary_box() {
  local color hex rgb IFS=' ' REPLY=

  [[ $# -gt 1 ]] || return 2
  color=$1
  shift

  hex=$(dot_ui_color_hex "$color") || return
  if _dot_ui_gum_bin; then
    "$REPLY" style --bold --foreground "$hex" --border rounded --padding '0 2' "$*"
  elif [[ -t 1 && -z ${NO_COLOR:-} ]]; then
    rgb=$(dot_ui_hex_to_rgb "$hex")
    printf '════════════════════════════════\n'
    printf '\033[1;38;2;%sm%s\033[0m\n' "$rgb" "$*"
    printf '════════════════════════════════\n'
  else
    printf '════════════════════════════════\n'
    printf '%s\n' "$*"
    printf '════════════════════════════════\n'
  fi
}
