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

# Helper failures run a failing call before their `return N`: under
# `set -e` the worker's failure trap then fires inside the helper and can
# name it, rather than reporting `return N` at the extension line. Where
# errexit is off (a condition, `set +e`) the explicit return keeps the
# documented status.
_dot_doctor_status() {
  return "$1"
}

dot_doctor_section() {
  [[ $# -eq 1 ]] || {
    _dot_doctor_status 2
    return 2
  }
  _dot_doctor_record section "$1"
}

dot_doctor_ok() {
  [[ $# -ge 1 && $# -le 2 ]] || {
    _dot_doctor_status 2
    return 2
  }
  _dot_doctor_record ok "$1" "${2:-}"
}

dot_doctor_warn() {
  [[ $# -ge 1 && $# -le 2 ]] || {
    _dot_doctor_status 2
    return 2
  }
  _dot_doctor_record warn "$1" "${2:-}"
}

dot_doctor_fail() {
  [[ $# -ge 1 && $# -le 2 ]] || {
    _dot_doctor_status 2
    return 2
  }
  _dot_doctor_record fail "$1" "${2:-}"
}

dot_doctor_skip() {
  [[ $# -ge 1 && $# -le 2 ]] || {
    _dot_doctor_status 2
    return 2
  }
  _dot_doctor_record skip "$1" "${2:-}"
}

# Newer than the other result helpers: an older coordinator has no `info`
# kind. Probe with `declare -F dot_doctor_info` and fall back to
# `dot_doctor_ok` so one extension works against either coordinator.
dot_doctor_info() {
  [[ $# -ge 1 && $# -le 2 ]] || {
    _dot_doctor_status 2
    return 2
  }
  _dot_doctor_record info "$1" "${2:-}"
}

# Attachments, newer than dot_doctor_info: each attaches TEXT to the
# closest ok/warn/fail/skip/info row filed before it by this extension.
# Items render as an indented list (the first few, then "+N more"); hints
# render as "→ TEXT" next-step lines. An older coordinator has neither, so
# probe with `declare -F dot_doctor_item` (or `dot_doctor_hint`) and fall
# back to joining the text into the row's detail.
dot_doctor_item() {
  [[ $# -eq 1 ]] || {
    _dot_doctor_status 2
    return 2
  }
  _dot_doctor_record item "$1"
}

dot_doctor_hint() {
  [[ $# -eq 1 ]] || {
    _dot_doctor_status 2
    return 2
  }
  _dot_doctor_record hint "$1"
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

  [[ $# -eq 1 ]] || {
    _dot_doctor_status 2
    return 2
  }
  case $relative in
    '' | /* | . | .. | ./* | ../* | */./* | */../* | */. | */.. | */ | *//* | *$'\n'* | *$'\r'*)
      _dot_doctor_status 2
      return 2
      ;;
  esac
  path=$DOT_EXTENSIONS_DIR/$relative
  _dot_extension_file_validate "$path" || {
    _dot_doctor_status 1
    return 1
  }
  set --
  # shellcheck source=/dev/null
  . "$path" || {
    _dot_doctor_status 1
    return 1
  }
}
