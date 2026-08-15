# shellcheck shell=bash
# Public doctor extension API 1. Extensions report records; the coordinator is
# the only process that renders output or mutates aggregate counters.

_dot_doctor_record() {
  local kind=$1 message=$2 detail=${3:-}

  [[ -n ${DOT_DOCTOR_RESULT_FILE:-} && -f $DOT_DOCTOR_RESULT_FILE ]] || return 1
  [[ $message != *$'\t'* && $message != *$'\n'* && $message != *$'\r'* ]] ||
    return 2
  [[ $detail != *$'\t'* && $detail != *$'\n'* && $detail != *$'\r'* ]] ||
    return 2
  printf '%s\t%s\t%s\n' "$kind" "$message" "$detail" \
    >>"$DOT_DOCTOR_RESULT_FILE"
}

dot_doctor_section() {
  [[ $# -eq 1 ]] || return 2
  _dot_doctor_record section "$1"
}

dot_doctor_ok() {
  [[ $# -ge 1 && $# -le 2 ]] || return 2
  _dot_doctor_record ok "$1" "${2:-}"
}

dot_doctor_warn() {
  [[ $# -ge 1 && $# -le 2 ]] || return 2
  _dot_doctor_record warn "$1" "${2:-}"
}

dot_doctor_fail() {
  [[ $# -ge 1 && $# -le 2 ]] || return 2
  _dot_doctor_record fail "$1" "${2:-}"
}

dot_doctor_skip() {
  [[ $# -ge 1 && $# -le 2 ]] || return 2
  _dot_doctor_record skip "$1" "${2:-}"
}

dot_doctor_display_path() {
  local path

  [[ $# -eq 1 ]] || return 2
  path=$1
  # shellcheck disable=SC2088 # Tilde is deliberate display text, not expansion.
  if [[ $HOME == / ]]; then
    case $path in
      /) printf '~\n' ;;
      /*) printf '~/%s\n' "${path#/}" ;;
      *) printf '%s\n' "$path" ;;
    esac
  else
    case $path in
      "$HOME") printf '~\n' ;;
      "$HOME"/*) printf '~/%s\n' "${path#"$HOME"/}" ;;
      *) printf '%s\n' "$path" ;;
    esac
  fi
}

dot_doctor_source() {
  local relative=${1:-} path

  [[ $# -eq 1 ]] || return 2
  case $relative in
    '' | /* | . | .. | ./* | ../* | */./* | */../* | */. | */.. | */ | *//* | *$'\n'* | *$'\r'*)
      return 2
      ;;
  esac
  path=$DOT_EXTENSIONS_DIR/$relative
  _dot_extension_file_validate "$path" || return 1
  set --
  # shellcheck source=/dev/null
  . "$path" || return 1
}
