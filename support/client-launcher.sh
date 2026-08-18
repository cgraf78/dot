#!/usr/bin/env bash
# Generated client-owned adapter for deployments that retain a regular
# ~/.local/bin/dot file. During the migration window it prefers the validated
# standalone checkout and otherwise invokes the retained embedded launcher.
# After that fallback is retired, the same bytes print the recovery installer.

set -euo pipefail
CDPATH=
shopt -u nocasematch 2>/dev/null || true

dot_client_error() {
  printf 'dot: %s\n' "$*" >&2
}

dot_client_normalized_absolute() {
  case $1 in
    '' | / | */ | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
      return 1
      ;;
    /*) return 0 ;;
    *) return 1 ;;
  esac
}

dot_client_state_home() {
  local value=${XDG_STATE_HOME:-}

  case $value in
    /) DOT_CLIENT_STATE_HOME=/ ;;
    /*)
      dot_client_normalized_absolute "$value" || return 1
      DOT_CLIENT_STATE_HOME=$value
      ;;
    *)
      DOT_CLIENT_STATE_HOME=$HOME/.local/state
      dot_client_normalized_absolute "$DOT_CLIENT_STATE_HOME"
      ;;
  esac
}

dot_client_private_file() {
  local path=$1 output uid mode links

  [[ -f $path && ! -L $path ]] || return 1
  if output=$(stat -c '%u %a %h' "$path" 2>/dev/null); then
    :
  elif output=$(stat -f '%u %Lp %l' "$path" 2>/dev/null); then
    :
  else
    return 1
  fi
  read -r uid mode links <<<"$output"
  [[ $uid == "$(id -u)" && $mode == 600 && $links == 1 ]]
}

dot_client_config_path() {
  local value=${XDG_CONFIG_HOME:-}

  case $value in
    /) DOT_CLIENT_CONFIG_PATH=/dot/config ;;
    /*)
      dot_client_normalized_absolute "$value" || return 1
      DOT_CLIENT_CONFIG_PATH=$value/dot/config
      ;;
    *)
      DOT_CLIENT_CONFIG_PATH=$HOME/.config/dot/config
      dot_client_normalized_absolute "$DOT_CLIENT_CONFIG_PATH"
      ;;
  esac
}

dot_client_resolve_checkout() {
  local install_home=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}

  while [[ $install_home != / && $install_home == */ ]]; do
    install_home=${install_home%/}
  done
  case $install_home in
    '' | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
      return 1
      ;;
    /) DOT_CLIENT_CHECKOUT=/cgraf78/dot ;;
    /*) DOT_CLIENT_CHECKOUT=$install_home/cgraf78/dot ;;
    *) return 1 ;;
  esac
  DOT_CLIENT_RUNTIME=$DOT_CLIENT_CHECKOUT/bin/dot
}

dot_client_read_lock() {
  local LC_ALL=C
  local lock=$1 size header phase_line revision_line generation_line extra=''

  [[ -f "$lock" && ! -L "$lock" ]] || return 1
  size=$(wc -c <"$lock" 2>/dev/null) || return 2
  size=${size//[[:space:]]/}
  case $size in
    '' | *[!0-9]*) return 2 ;;
  esac
  [[ ${#size} -le 4 && $size -le 1024 ]] || return 2
  {
    IFS= read -r header || return 2
    IFS= read -r phase_line || return 2
    IFS= read -r revision_line || return 2
    IFS= read -r generation_line || return 2
    if IFS= read -r extra || [[ -n $extra ]]; then
      return 2
    fi
  } <"$lock"
  [[ $header == 'cgraf78 dot client cutover v4' &&
    $phase_line == phase=* &&
    $revision_line == minimum_revision=* &&
    $generation_line == readiness_generation=* ]] || return 2
  DOT_CLIENT_PHASE=${phase_line#phase=}
  case $DOT_CLIENT_PHASE in
    prepare | active) ;;
    *) return 2 ;;
  esac
  DOT_CLIENT_MINIMUM_REVISION=${revision_line#minimum_revision=}
  [[ $DOT_CLIENT_MINIMUM_REVISION =~ ^[0-9a-f]{40}$ ]] || return 2
  DOT_CLIENT_READINESS_GENERATION=${generation_line#readiness_generation=}
  [[ $DOT_CLIENT_READINESS_GENERATION =~ ^[0-9a-z][0-9a-z-]{0,63}$ ]] || return 2
}

dot_client_git() (
  unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_OBJECT_DIRECTORY
  unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_INDEX_FILE GIT_CONFIG
  unset GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_COUNT
  exec "$DOT_CLIENT_GIT" "$@"
)

dot_client_exact_file() {
  local expected=$1 actual=$2

  # Git is the launcher's existing trust primitive and is present on minimal
  # bootstrap images that omit coreutils `cmp`. Suppress configurable diff
  # delegates so this remains a literal byte comparison.
  dot_client_git --no-pager diff --no-index --quiet --no-ext-diff --no-textconv \
    -- "$expected" "$actual"
}

dot_client_exact_link() {
  local path=$1 expected=$2 actual

  [[ -L $path ]] || return 1
  actual=$(readlink "$path") || return 1
  [[ $actual == "$expected" ]]
}

dot_client_read_ready() {
  local ready=$1 size header config_line oid_line extra='' actual_oid

  dot_client_private_file "$ready" || return 1
  size=$(wc -c <"$ready" 2>/dev/null) || return 1
  size=${size//[[:space:]]/}
  case $size in
    '' | *[!0-9]*) return 1 ;;
  esac
  [[ ${#size} -le 4 && $size -le 4096 ]] || return 1
  {
    IFS= read -r header || return 1
    IFS= read -r config_line || return 1
    IFS= read -r oid_line || return 1
    if IFS= read -r extra || [[ -n $extra ]]; then
      return 1
    fi
  } <"$ready"
  [[ $header == 'cgraf78 dot client readiness v4' &&
    $config_line == config_path=* && $oid_line == config_oid=* ]] || return 1
  [[ ${config_line#config_path=} == "$DOT_CLIENT_CONFIG_PATH" ]] || return 1
  DOT_CLIENT_CONFIG_OID=${oid_line#config_oid=}
  [[ $DOT_CLIENT_CONFIG_OID =~ ^[0-9a-f]{40}$ ]] || return 1
  actual_oid=$(dot_client_git -C "$DOT_CLIENT_CHECKOUT" hash-object \
    --no-filters -- "$DOT_CLIENT_CONFIG_PATH" 2>/dev/null) || return 1
  [[ $actual_oid == "$DOT_CLIENT_CONFIG_OID" ]]
}

dot_client_standalone_ready() {
  local checkout=$1 runtime=$2 launcher=$3 ready=$4 template
  local checkout_physical top top_physical head

  [[ -f "$runtime" && ! -L "$runtime" && -x "$runtime" ]] || return 1
  template=$checkout/support/client-launcher.sh
  [[ -f "$template" && ! -L "$template" ]] || return 1
  DOT_CLIENT_GIT=$(type -P git 2>/dev/null) || return 1
  dot_client_normalized_absolute "$DOT_CLIENT_GIT" || return 1
  [[ -f "$DOT_CLIENT_GIT" && -x "$DOT_CLIENT_GIT" ]] || return 1
  dot_client_config_path || return 1
  dot_client_read_ready "$ready" || return 1
  dot_client_exact_file "$launcher" "$template" || return 1
  dot_client_exact_link "$HOME/.local/lib/dot" "$checkout/lib/dot/public" ||
    return 1
  checkout_physical=$(cd -P -- "$checkout" 2>/dev/null && pwd -P) || return 1
  top=$(dot_client_git -C "$checkout" rev-parse --show-toplevel 2>/dev/null) ||
    return 1
  top_physical=$(cd -P -- "$top" 2>/dev/null && pwd -P) || return 1
  [[ $top_physical == "$checkout_physical" ]] || return 1
  head=$(dot_client_git -C "$checkout" rev-parse --verify 'HEAD^{commit}' 2>/dev/null) ||
    return 1
  [[ $head =~ ^[0-9a-f]{40}$ ]] || return 1
  if ! dot_client_git -C "$checkout" merge-base --is-ancestor \
    "$DOT_CLIENT_MINIMUM_REVISION" "$head" >/dev/null 2>&1; then
    return 1
  fi
}

[[ -n ${HOME:-} ]] || {
  dot_client_error 'HOME is not set'
  exit 1
}

launcher_parent=$(cd -P -- "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd -P) || {
  dot_client_error 'cannot resolve the client launcher'
  exit 1
}
launcher=$launcher_parent/${BASH_SOURCE[0]##*/}
[[ -f "$launcher" && ! -L "$launcher" ]] || {
  dot_client_error "client launcher is not a regular file: $launcher"
  exit 1
}

cutover_lock=${DOT_CLIENT_CUTOVER_LOCK:-$HOME/.local/lib/dotfiles/dot-cutover.lock}
handoff_helper=${DOT_CLIENT_HANDOFF_HELPER:-$HOME/.local/lib/dotfiles/dot-library-handoff.sh}
legacy_launcher=${DOT_CLIENT_LEGACY_LAUNCHER:-$HOME/.local/lib/dotfiles/legacy-dot-launcher.sh}
ready_override=${DOT_CLIENT_READY:-}
for client_path in "$cutover_lock" "$handoff_helper" "$legacy_launcher"; do
  dot_client_normalized_absolute "$client_path" || {
    dot_client_error "client migration path must be normalized and absolute: $client_path"
    exit 1
  }
done

if [[ -e "$handoff_helper" || -L "$handoff_helper" ]]; then
  [[ -f "$handoff_helper" && ! -L "$handoff_helper" && -x "$handoff_helper" ]] || {
    dot_client_error "library handoff helper is unsafe: $handoff_helper"
    exit 1
  }
fi

standalone_status=0
standalone_authorized=0
dot_client_read_lock "$cutover_lock" || standalone_status=$?
ready=''
checkout=''
dot_runtime=''
# The tracked phase is the fleet-visible deployment gate. `prepare` may stage a
# complete checkout, but cannot activate it. An embedded updater also re-execs
# this front door with its private --skip-pull flag; keep that exact invocation
# on the retained launcher because standalone Dot deliberately does not own the
# old engine's private flag.
if [[ $standalone_status -eq 0 && $DOT_CLIENT_PHASE == active &&
  ${DOT_REEXEC:-0} != 1 ]]; then
  if [[ -n $ready_override ]]; then
    if dot_client_normalized_absolute "$ready_override"; then
      ready=$ready_override
    else
      standalone_status=1
    fi
  elif dot_client_state_home; then
    # The symbolic generation lets a client invalidate obsolete convergence
    # proofs when its topology changes without inventing a second runtime pin.
    if [[ $DOT_CLIENT_STATE_HOME == / ]]; then
      ready=/dot/client-ready-v4/$DOT_CLIENT_READINESS_GENERATION/$DOT_CLIENT_MINIMUM_REVISION
    else
      ready=$DOT_CLIENT_STATE_HOME/dot/client-ready-v4/$DOT_CLIENT_READINESS_GENERATION/$DOT_CLIENT_MINIMUM_REVISION
    fi
  else
    standalone_status=1
  fi
  if [[ $standalone_status -eq 0 ]] &&
    dot_client_resolve_checkout &&
    dot_client_standalone_ready \
      "$DOT_CLIENT_CHECKOUT" "$DOT_CLIENT_RUNTIME" "$launcher" "$ready"; then
    checkout=$DOT_CLIENT_CHECKOUT
    dot_runtime=$DOT_CLIENT_RUNTIME
    standalone_authorized=1
  fi
fi
if [[ $standalone_authorized -eq 1 ]]; then
  exec "$dot_runtime" "$@"
fi

if [[ -f "$legacy_launcher" && ! -L "$legacy_launcher" &&
  -x "$legacy_launcher" ]]; then
  if [[ -f "$handoff_helper" && ! -L "$handoff_helper" &&
    -x "$handoff_helper" ]]; then
    "$handoff_helper" restore || {
      dot_client_error 'library handoff restoration failed'
      exit 1
    }
  fi
  DOT_CLIENT_FRONT_DOOR=$launcher
  export DOT_CLIENT_FRONT_DOOR
  exec "$legacy_launcher" "$@"
fi
if [[ -e "$legacy_launcher" || -L "$legacy_launcher" ]]; then
  dot_client_error "embedded launcher is unsafe: $legacy_launcher"
  exit 1
fi

if [[ $standalone_status -eq 2 ]]; then
  dot_client_error "malformed cutover lock: $cutover_lock"
  exit 2
fi
if [[ -z $dot_runtime ]]; then
  dot_client_resolve_checkout || {
    dot_client_error 'SHDEPS_INSTALL_DIR must be normalized and absolute'
    exit 1
  }
  dot_runtime=$DOT_CLIENT_RUNTIME
fi

cat >&2 <<EOF
dot: standalone runtime is unavailable: $dot_runtime
reinstall it with:
  curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh | bash
EOF
exit 1
