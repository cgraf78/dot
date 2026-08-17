#!/usr/bin/env bash
# Generated client-owned adapter for deployments that retain a regular
# ~/.local/bin/dot file. During the migration window it prefers the validated
# standalone checkout and otherwise invokes the retained embedded launcher.
# After that fallback is retired, the same bytes print the recovery installer.

set -euo pipefail
CDPATH=

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

dot_client_read_lock() {
  local lock=$1 size header phase_line revision_line extra=''

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
    if IFS= read -r extra || [[ -n $extra ]]; then
      return 2
    fi
  } <"$lock"
  [[ $header == 'cgraf78 dot client cutover v2' &&
    $phase_line == phase=* &&
    $revision_line == minimum_revision=* ]] || return 2
  DOT_CLIENT_PHASE=${phase_line#phase=}
  case $DOT_CLIENT_PHASE in
    prepare | active) ;;
    *) return 2 ;;
  esac
  DOT_CLIENT_MINIMUM_REVISION=${revision_line#minimum_revision=}
  [[ $DOT_CLIENT_MINIMUM_REVISION =~ ^[0-9a-f]{40}$ ]] || return 2
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

dot_client_standalone_ready() {
  local checkout=$1 runtime=$2 launcher=$3 template
  local checkout_physical top top_physical head

  [[ -f "$runtime" && ! -L "$runtime" && -x "$runtime" ]] || return 1
  template=$checkout/support/client-launcher.sh
  [[ -f "$template" && ! -L "$template" ]] || return 1
  DOT_CLIENT_GIT=$(type -P git 2>/dev/null) || return 1
  dot_client_normalized_absolute "$DOT_CLIENT_GIT" || return 1
  [[ -f "$DOT_CLIENT_GIT" && -x "$DOT_CLIENT_GIT" ]] || return 1
  dot_client_exact_file "$launcher" "$template" || return 1
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
state_home=${XDG_STATE_HOME:-$HOME/.local/state}
for client_path in "$cutover_lock" "$handoff_helper" "$legacy_launcher" "$state_home"; do
  dot_client_normalized_absolute "$client_path" || {
    dot_client_error "client migration path must be normalized and absolute: $client_path"
    exit 1
  }
done
if [[ -n $ready_override ]]; then
  dot_client_normalized_absolute "$ready_override" || {
    dot_client_error "client readiness path must be normalized and absolute: $ready_override"
    exit 1
  }
fi

if [[ -e "$handoff_helper" || -L "$handoff_helper" ]]; then
  [[ -f "$handoff_helper" && ! -L "$handoff_helper" && -x "$handoff_helper" ]] || {
    dot_client_error "library handoff helper is unsafe: $handoff_helper"
    exit 1
  }
fi

install_home=${SHDEPS_INSTALL_DIR:-$HOME/.local/share}
while [[ "$install_home" != / && "$install_home" == */ ]]; do
  install_home=${install_home%/}
done
case $install_home in
  '' | *//* | */./* | */. | */../* | */.. | *$'\n'* | *$'\r'*)
    dot_client_error 'SHDEPS_INSTALL_DIR must be normalized'
    exit 1
    ;;
  /) checkout=/cgraf78/dot ;;
  /*) checkout=$install_home/cgraf78/dot ;;
  *)
    dot_client_error 'SHDEPS_INSTALL_DIR must be an absolute path'
    exit 1
    ;;
esac
dot_runtime=$checkout/bin/dot

standalone_status=0
standalone_authorized=0
dot_client_read_lock "$cutover_lock" || standalone_status=$?
if [[ $standalone_status -eq 2 ]]; then
  dot_client_error "malformed cutover lock: $cutover_lock"
  exit 2
fi
ready=''
if [[ $standalone_status -eq 0 ]]; then
  ready=${ready_override:-$state_home/dot/client-ready-v2/$DOT_CLIENT_MINIMUM_REVISION}
fi
# The tracked phase is the fleet-visible deployment gate. `prepare` may stage a
# complete checkout, but cannot activate it. An embedded updater also re-execs
# this front door with its private --skip-pull flag; keep that exact invocation
# on the retained launcher because standalone Dot deliberately does not own the
# old engine's private flag.
if [[ $standalone_status -eq 0 && $DOT_CLIENT_PHASE == active &&
  ${DOT_REEXEC:-0} != 1 && -d $ready && ! -L $ready ]]; then
  if dot_client_standalone_ready "$checkout" "$dot_runtime" "$launcher"; then
    standalone_authorized=1
  else
    standalone_status=$?
  fi
fi
case $standalone_status in
  0)
    if [[ $standalone_authorized -eq 1 ]]; then
      exec "$dot_runtime" "$@"
    fi
    ;;
  1) ;;
  *) exit "$standalone_status" ;;
esac

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

cat >&2 <<EOF
dot: standalone runtime is unavailable: $dot_runtime
reinstall it with:
  curl -fsSL https://raw.githubusercontent.com/cgraf78/dot/main/install.sh | bash
EOF
exit 1
