# shellcheck shell=bash
# Public XDG base-directory resolver. Empty and relative XDG values fall back
# to HOME because accepting them would make runtime ownership depend on cwd.

dot_xdg_home() {
  local kind value fallback

  REPLY=
  [[ $# -eq 1 ]] || return 2
  kind=$1

  if test "$kind" = config; then
    value=${XDG_CONFIG_HOME:-}
    fallback=.config
  elif test "$kind" = state; then
    value=${XDG_STATE_HOME:-}
    fallback=.local/state
  elif test "$kind" = cache; then
    value=${XDG_CACHE_HOME:-}
    fallback=.cache
  elif test "$kind" = data; then
    value=${XDG_DATA_HOME:-}
    fallback=.local/share
  else
    return 2
  fi

  case $value in
    /*)
      REPLY=$value
      return 0
      ;;
  esac
  case ${HOME:-} in
    /)
      REPLY=/$fallback
      return 0
      ;;
    /*)
      REPLY=$HOME/$fallback
      return 0
      ;;
  esac
  return 1
}

dot_xdg_path() {
  local kind suffix base

  REPLY=
  [[ $# -eq 2 ]] || return 2
  kind=$1
  suffix=$2
  case $suffix in
    '' | /* | */ | *//* | . | ./* | */./* | */. | .. | ../* | */../* | */.. | *$'\n'* | *$'\r'*)
      return 2
      ;;
  esac

  dot_xdg_home "$kind" || return
  base=$REPLY
  if [[ $base == / ]]; then
    REPLY=/$suffix
  else
    REPLY=$base/$suffix
  fi
}
